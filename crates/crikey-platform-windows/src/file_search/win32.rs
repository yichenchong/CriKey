//! The `SystemIndex` catalog, reached the way Explorer reaches it.
//!
//! # The call sequence
//!
//! `CoCreateInstance(CSearchManager)` -> [`ISearchManager::GetCatalog`] with
//! `"SystemIndex"` -> [`ISearchCatalogManager::GetCatalogStatus`] to find out
//! whether the catalog can answer at all ->
//! [`ISearchCatalogManager::GetQueryHelper`] ->
//! [`ISearchQueryHelper::ConnectionString`], which returns
//! `provider=Search.CollatorDSO;EXTENDED PROPERTIES="Application=Windows"`.
//! That string initialises an OLE DB data source through `IDataInitialize`, and
//! the rest is an ordinary read-only OLE DB `SELECT`:
//! `IDBInitialize::Initialize` -> `IDBCreateSession::CreateSession` ->
//! `IDBCreateCommand::CreateCommand` -> `ICommandText::SetCommandText` with the
//! `DBGUID_DEFAULT` dialect -> `ICommand::Execute` for an `IRowset` -> one
//! `IAccessor` accessor over [`Row`] -> `GetNextRows`/`GetData` until the rowset
//! is drained.
//!
//! `ISearchDesktop` is the older way to do this and is deprecated; it is not
//! used. The query runs in the caller's security context and Windows Search
//! trims rows the caller may not see, so this needs no elevation and grants the
//! process nothing it did not already have.
//!
//! # Why the query runs on its own thread
//!
//! `ICommand::Execute` is synchronous, and Microsoft publishes no latency figure
//! for it -- their own guidance is that catalogs past roughly 400,000 items may
//! misbehave. The shared contract says the deadline is a promise, so the caller
//! cannot be the thread that waits on the provider. The query therefore runs on
//! a thread that streams each fetched batch back over a channel, and the caller
//! collects batches until the deadline and then stops listening. An abandoned
//! query finishes into a closed channel and the thread exits; the next keystroke
//! does not wait for it.
//!
//! # What a cancellation can actually stop here
//!
//! Three different things, and it is worth being exact about which:
//!
//! * **Before any COM work**, a cancelled token means the search never starts:
//!   no activation, no apartment, no thread.
//! * **The caller's drain loop** stops within [`CANCEL_POLL`] and returns the
//!   batches it already has as [`FileSearchCoverage::Cancelled`].
//! * **The worker** stops at a *batch boundary*: it checks the token before each
//!   `GetNextRows` and after each send, so it stops fetching instead of draining
//!   the rest of the rowset.
//!
//! What cannot be stopped is a call already inside the provider. `GetNextRows`
//! is synchronous and has no documented mid-call abort; `ICommand::Cancel`
//! applies only between `Execute` being entered and returning, and
//! `IDBAsynchStatus::Abort` covers asynchronous rowset population, which this
//! module does not request. So a cancellation arriving during a fetch is
//! honoured when that fetch returns, not before.
//!
//! Abandoning the rowset part-way is nonetheless safe: a final release of the
//! rowset's interface pointers cleans up the row handles subordinate to it, and
//! this module releases each batch's handles with `ReleaseRows` and frees the
//! provider-allocated handle array with the COM task allocator as it goes, so
//! the cancellation path leaves nothing outstanding. The accessor is the one
//! object that needs its own `ReleaseAccessor`, which is what [`Accessor`]'s
//! `Drop` is for, and every release happens on the thread and in the apartment
//! that created the objects, as COM requires.
//!
//! # Why there is no MFT enumeration here
//!
//! See the module documentation of [`super`]: `FSCTL_ENUM_USN_DATA` and
//! `FSCTL_READ_USN_JOURNAL` need administrator privileges, and a launcher that
//! needs elevation to search for a file is not one this codebase ships.

#![allow(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStringExt;
use std::ptr;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use windows::core::{w, Error, IUnknown, Interface, GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Search::{
    CSearchManager, IAccessor, ICommandText, IDBCreateCommand, IDBCreateSession, IDBInitialize,
    IDataInitialize, IRowset, ISearchCatalogManager, ISearchManager, CATALOG_PAUSED_REASON_NONE,
    CATALOG_STATUS_FULL_CRAWL, CATALOG_STATUS_IDLE, CATALOG_STATUS_INCREMENTAL_CRAWL,
    CATALOG_STATUS_PROCESSING_NOTIFICATIONS, DBACCESSOR_ROWDATA, DBBINDING, DBMEMOWNER_CLIENTOWNED,
    DBPARAMIO_NOTPARAM, DBPART_LENGTH, DBPART_STATUS, DBPART_VALUE, DBSTATUS_S_OK, DBTYPE_FILETIME,
    DBTYPE_UI4, DBTYPE_WSTR, HACCESSOR, MSDAINITIALIZE,
};

use crikey_core::{CoreError, PlatformPath, Result};
use crikey_platform::{CancelToken, FileHit, FileKind, FileSearchCoverage, FileSearchResults};

use crate::win32::Apartment;

/// `DBGUID_DEFAULT` from `oledbguid.h`, the dialect every OLE DB provider that
/// supports commands must accept.
///
/// Written out because the `windows` crate generates the OLE DB interfaces and
/// enumerations but not this GUID; the value is the one the header defines,
/// where it is also the value of `DBGUID_DBSQL`.
const DBGUID_DEFAULT: GUID = GUID::from_u128(0xc8b521fb_5cf3_11ce_ade5_00aa0044773d);

/// How many rows one `GetNextRows` asks for.
///
/// Large enough that a full answer costs a handful of round trips, small enough
/// that the caller sees the first batch long before the last one and can stop at
/// its deadline with real hits in hand.
const BATCH: usize = 32;

/// Longest the caller waits on the channel before looking at the cancellation
/// token again.
///
/// The deadline is generous by design -- a second is reasonable, because the
/// provider runs off the UI thread -- so a plain `recv_timeout(remaining)` would
/// leave a cancelled search holding its batches until the provider next spoke.
/// Slicing the wait bounds that to this interval, which is well under the delay
/// a user could perceive between keystrokes, at a cost of at most a couple of
/// hundred timer wakeups across a whole one-second deadline.
const CANCEL_POLL: Duration = Duration::from_millis(5);

/// UTF-16 code units reserved for a bound `System.ItemPathDisplay`.
///
/// `MAX_PATH` is 260, but the catalog holds paths from volumes where the long
/// path limit applies, so the buffer is sized for those rather than for the
/// legacy limit. A path longer than this arrives with a truncated status and is
/// dropped: a path that cannot be opened is worse than a row that is missing.
const PATH_UNITS: usize = 1024;
/// UTF-16 code units reserved for a bound `System.FileName`.
const NAME_UNITS: usize = 512;
/// UTF-16 code units reserved for a bound `System.ItemType`.
///
/// The value is a file extension or the literal `Directory`, so this is
/// generous.
const TYPE_UNITS: usize = 64;

/// What the query thread has to say to the caller.
enum Message {
    /// Hits from one `GetNextRows` batch.
    Rows(Vec<FileHit>),
    /// The rowset was drained; there is nothing further to come.
    Finished,
    /// The catalog could not be queried. Meaningful only before the first batch:
    /// afterwards the rows already sent are a real, if short, answer.
    Refused(CoreError),
}

/// The catalog's answer to `sql`, or the reason it is not a source.
///
/// `Err` is the signal to fall back to a directory walk, and it carries why:
/// the Search service would not talk, its catalog status says it cannot answer
/// right now, or the query failed before returning a single row. The caller
/// surfaces that reason only when it has nowhere to walk either, which is the
/// one case where the user would otherwise be told nothing at all. A catalog
/// that answers with no rows returns `Ok` with an empty hit list, because "this
/// index contains nothing matching" is an answer.
///
/// The deadline is measured from `started`, so a caller that has already spent
/// part of its budget gets what the catalog can produce in the remainder.
pub(super) fn search(
    sql: &str,
    started: Instant,
    deadline: Duration,
    cancel: &CancelToken,
    limit: usize,
) -> Result<FileSearchResults> {
    // Cheapest possible honesty: a search superseded before it began does not
    // activate the Search service, does not enter an apartment and does not
    // spawn a thread whose COM teardown someone would have to wait for.
    if cancel.is_cancelled() {
        return Ok(FileSearchResults {
            hits: Vec::new(),
            coverage: FileSearchCoverage::Cancelled,
        });
    }

    let (sender, receiver) = mpsc::channel();
    let owned_sql = sql.to_owned();
    let worker_cancel = cancel.clone();
    // A named thread so a stuck provider is identifiable in a debugger or a
    // crash dump rather than being one more anonymous worker.
    thread::Builder::new()
        .name("crikey-file-search".to_owned())
        .spawn(move || {
            let message = match query(&owned_sql, limit, &worker_cancel, &sender) {
                Ok(()) => Message::Finished,
                Err(refusal) => Message::Refused(refusal),
            };
            // A closed channel means the caller stopped listening at its
            // deadline. There is nobody to tell, and that is not an error.
            let _ = sender.send(message);
        })
        .map_err(|error| {
            CoreError::Invalid(format!(
                "the Windows Search index cannot be queried without a thread to wait on it: {error}"
            ))
        })?;

    let mut hits: Vec<FileHit> = Vec::new();
    let mut coverage = FileSearchCoverage::Partial;
    loop {
        // Cancellation is decided here rather than inside the `recv_timeout`
        // arm so that it is checked once per slice however the wait ended, and
        // ahead of the deadline because a caller who gave up is told that
        // rather than that the clock ran out.
        if cancel.is_cancelled() {
            coverage = FileSearchCoverage::Cancelled;
            break;
        }
        let remaining = deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            coverage = FileSearchCoverage::Deadline;
            break;
        }
        match receiver.recv_timeout(remaining.min(CANCEL_POLL)) {
            Ok(Message::Rows(mut batch)) => hits.append(&mut batch),
            Ok(Message::Finished) => break,
            // Before any row, a refusal means the catalog cannot answer and the
            // walk must. After one, the rows already collected are the catalog's
            // answer, cut short, and reporting a failure would throw them away.
            Ok(Message::Refused(refusal)) => {
                if hits.is_empty() {
                    return Err(refusal);
                }
                break;
            }
            // The query thread died without a word, which is not something a
            // provider is supposed to do.
            Err(RecvTimeoutError::Disconnected) => {
                if hits.is_empty() {
                    return Err(CoreError::Invalid(
                        "the Windows Search query ended without reporting an outcome".to_owned(),
                    ));
                }
                break;
            }
            // One poll slice with nothing to show for it. Whether that is the
            // end of the wait is the top of the loop's decision.
            Err(RecvTimeoutError::Timeout) => continue,
        }
        if hits.len() >= limit {
            break;
        }
    }
    hits.truncate(limit);

    Ok(FileSearchResults { hits, coverage })
}

/// Every failure in this module means the same thing to the caller -- the
/// catalog cannot answer, so something else must -- but the words Windows used
/// are kept for `crikey plugin doctor`.
fn refused(error: Error) -> CoreError {
    crate::win32::refused("query its own search index", &error)
}

/// Runs `sql` against `SystemIndex`, sending each fetched batch to `sender`.
///
/// Returns once the rowset is drained, `sender` is closed, `cancel` is set, or
/// `limit` hits have been sent. Every COM object is created and released inside
/// this function, on this thread, because none of them may cross an apartment
/// boundary.
///
/// `cancel` is polled at the batch boundary and nowhere finer, which is the
/// honest limit of this path: `ICommand::Execute` and `IRowset::GetNextRows` are
/// synchronous calls into the provider, and a call already inside the Search
/// service cannot be taken back.
fn query(sql: &str, limit: usize, cancel: &CancelToken, sender: &Sender<Message>) -> Result<()> {
    let _apartment = Apartment::enter("a Windows Search query")?;

    // SAFETY: an in-process activation of a documented shell class; the class id
    // and the interface the result is bound to are Microsoft's own.
    let manager: ISearchManager =
        unsafe { CoCreateInstance(&CSearchManager, None, CLSCTX_INPROC_SERVER) }.map_err(refused)?;
    // SAFETY: `SystemIndex` is a NUL-terminated literal that outlives the call.
    let catalog = unsafe { manager.GetCatalog(w!("SystemIndex")) }.map_err(refused)?;

    if !answers_queries(&catalog) {
        return Err(CoreError::Invalid(
            "the Windows Search catalog is paused, recovering or shutting down and cannot answer \
             a query"
                .to_owned(),
        ));
    }

    // SAFETY: no arguments; the helper is a new object owned by this frame.
    let helper = unsafe { catalog.GetQueryHelper() }.map_err(refused)?;
    // SAFETY: an out parameter the provider allocates and `CoString` frees.
    let connection = CoString(unsafe { helper.ConnectionString() }.map_err(refused)?);

    // SAFETY: as above, for the OLE DB service component.
    let initialiser: IDataInitialize =
        unsafe { CoCreateInstance(&MSDAINITIALIZE, None, CLSCTX_INPROC_SERVER) }.map_err(refused)?;
    let mut source: Option<IUnknown> = None;
    // SAFETY: `connection` owns the string for the duration of the call, and
    // `source` is an out parameter this frame owns.
    unsafe {
        initialiser.GetDataSource(
            None,
            CLSCTX_INPROC_SERVER.0,
            PCWSTR(connection.0.as_ptr()),
            &IDBInitialize::IID,
            &mut source,
        )
    }
    .map_err(refused)?;
    let source = source.ok_or_else(|| {
        CoreError::Invalid(
            "the OLE DB service component reported success without returning a data source".to_owned(),
        )
    })?;

    let initialize: IDBInitialize = source.cast().map_err(refused)?;
    // SAFETY: no arguments; initialises the data source this frame just made.
    unsafe { initialize.Initialize() }.map_err(refused)?;
    let sessions: IDBCreateSession = initialize.cast().map_err(refused)?;
    // SAFETY: no aggregation, and the interface id is the one the result is cast
    // to immediately below.
    let session = unsafe { sessions.CreateSession(None, &IDBCreateCommand::IID) }.map_err(refused)?;
    let commands: IDBCreateCommand = session.cast().map_err(refused)?;
    // SAFETY: as above.
    let command = unsafe { commands.CreateCommand(None, &ICommandText::IID) }.map_err(refused)?;
    let text: ICommandText = command.cast().map_err(refused)?;

    let wide_sql = crate::win32::wide(OsStr::new(sql));
    // SAFETY: `wide_sql` is NUL terminated and outlives the call, and the
    // dialect is the one every command-capable provider accepts.
    unsafe { text.SetCommandText(&DBGUID_DEFAULT, PCWSTR(wide_sql.as_ptr())) }.map_err(refused)?;

    let mut rowset: Option<IUnknown> = None;
    // SAFETY: no parameters and no row count wanted; `rowset` is an out
    // parameter this frame owns.
    unsafe { text.Execute(None, &IRowset::IID, None, None, Some(&mut rowset)) }.map_err(refused)?;
    let rowset: IRowset = rowset
        .ok_or_else(|| {
            CoreError::Invalid(
                "the Windows Search provider reported success without returning a rowset".to_owned(),
            )
        })?
        .cast()
        .map_err(refused)?;

    let factory: IAccessor = rowset.cast().map_err(refused)?;
    let bindings = bindings();
    let mut handle = HACCESSOR(0);
    // SAFETY: `bindings` describes offsets inside a `Row` and lives until the
    // call returns; `cbrowsize` is that type's real size, so the provider cannot
    // write outside the buffer `GetData` is given.
    unsafe {
        factory.CreateAccessor(
            DBACCESSOR_ROWDATA.0 as u32,
            bindings.len(),
            bindings.as_ptr(),
            size_of::<Row>(),
            &mut handle,
            None,
        )
    }
    .map_err(refused)?;
    let accessor = Accessor {
        factory: &factory,
        handle,
    };

    let mut rows = vec![Row::EMPTY; BATCH];
    let mut sent = 0usize;
    while sent < limit {
        if cancel.is_cancelled() {
            break;
        }
        // The provider allocates the handle array and writes its address into
        // the first element; the slice length is how many rows are wanted.
        let mut handles = [ptr::null_mut::<usize>(); BATCH];
        let mut obtained = 0usize;
        // SAFETY: `handles` is `BATCH` elements long, which is the row count the
        // binding declares, and `obtained` is an out parameter this frame owns.
        if unsafe { rowset.GetNextRows(0, 0, &mut obtained, &mut handles) }.is_err() {
            // A provider that gives up mid-rowset has still told the truth about
            // the rows it did give.
            break;
        }
        // Zero rows is how a drained rowset reports itself: `DB_S_ENDOFROWSET`
        // is a success code, so the count is the end-of-data signal.
        if obtained == 0 {
            break;
        }
        let allocated = handles[0];

        let mut batch = Vec::with_capacity(obtained);
        for (index, row) in rows.iter_mut().take(obtained).enumerate() {
            // SAFETY: the provider reported `obtained` handles in the array it
            // allocated at `allocated`.
            let row_handle = unsafe { *allocated.add(index) };
            let buffer: *mut Row = row;
            // SAFETY: `buffer` is a whole `Row`, which is the row size the
            // accessor was created with.
            if unsafe { rowset.GetData(row_handle, accessor.handle, buffer.cast()) }.is_ok() {
                if let Some(hit) = row.hit() {
                    batch.push(hit);
                }
            }
        }

        // SAFETY: the same handles the provider just handed out, released
        // exactly once; no reference counts or statuses are wanted back.
        let _ =
            unsafe { rowset.ReleaseRows(obtained, allocated, ptr::null(), ptr::null_mut(), ptr::null_mut()) };
        // SAFETY: the array was allocated by the provider with the COM task
        // allocator, which is what frees it.
        unsafe { CoTaskMemFree(Some(allocated.cast())) };

        sent += batch.len();
        if sender.send(Message::Rows(batch)).is_err() {
            // The caller reached its deadline and stopped listening.
            break;
        }
        if cancel.is_cancelled() {
            // Checked again after the send because the batch just handed over is
            // work the caller may still want, while the next `GetNextRows` is
            // work nobody does. Stopping here leaves the rowset undrained on
            // purpose.
            break;
        }
        // A short batch means the rowset had no more to give.
        if obtained < BATCH {
            break;
        }
    }

    drop(accessor);
    Ok(())
}

/// Whether the catalog is in a state that answers queries.
///
/// Idle, both crawls and notification processing all answer: a catalog still
/// building returns fewer rows than it eventually will, which is coverage rather
/// than failure, and the coverage this backend reports is `Partial` either way.
/// Paused, recovering -- a rebuild after corruption -- and shutting down do not,
/// and asking anyway would return an emptiness the user would read as "no such
/// file". A catalog that will not describe its own status is treated the same,
/// because a service that cannot answer that will not answer a `SELECT` either.
///
/// [`ISearchCatalogManager::NumberOfItemsToIndex`] is deliberately not consulted:
/// it reports how far behind the crawler is, which changes how fresh the answer
/// is, not whether there is one, and freshness has no state in the shared
/// coverage enum beyond the `Partial` already reported.
fn answers_queries(catalog: &ISearchCatalogManager) -> bool {
    let mut status = CATALOG_STATUS_IDLE;
    let mut reason = CATALOG_PAUSED_REASON_NONE;
    // SAFETY: two out parameters this frame owns.
    if unsafe { catalog.GetCatalogStatus(&mut status, &mut reason) }.is_err() {
        return false;
    }

    status == CATALOG_STATUS_IDLE
        || status == CATALOG_STATUS_FULL_CRAWL
        || status == CATALOG_STATUS_INCREMENTAL_CRAWL
        || status == CATALOG_STATUS_PROCESSING_NOTIFICATIONS
}

/// One binding per column of [`super::SELECT_COLUMNS`], in that order.
///
/// Ordinals are one-based and follow the select list. Every column binds its
/// value, its length and its status, because a row can carry a null or a
/// truncated value for any of them and a value read without its status is a
/// guess. The memory is client owned and every buffer is a fixed-size array
/// inside [`Row`], so nothing here has to be freed per row.
fn bindings() -> [DBBINDING; 5] {
    [
        binding(
            1,
            offset_of!(Row, path),
            offset_of!(Row, path_length),
            offset_of!(Row, path_status),
            DBTYPE_WSTR.0 as u16,
            size_of::<[u16; PATH_UNITS]>(),
        ),
        binding(
            2,
            offset_of!(Row, name),
            offset_of!(Row, name_length),
            offset_of!(Row, name_status),
            DBTYPE_WSTR.0 as u16,
            size_of::<[u16; NAME_UNITS]>(),
        ),
        binding(
            3,
            offset_of!(Row, item_type),
            offset_of!(Row, item_type_length),
            offset_of!(Row, item_type_status),
            DBTYPE_WSTR.0 as u16,
            size_of::<[u16; TYPE_UNITS]>(),
        ),
        binding(
            4,
            offset_of!(Row, attributes),
            offset_of!(Row, attributes_length),
            offset_of!(Row, attributes_status),
            DBTYPE_UI4.0 as u16,
            size_of::<u32>(),
        ),
        binding(
            5,
            offset_of!(Row, modified),
            offset_of!(Row, modified_length),
            offset_of!(Row, modified_status),
            // `System.DateModified` is a `FILETIME` in the catalog, so binding it
            // as one is the conversion-free request.
            DBTYPE_FILETIME.0 as u16,
            size_of::<FILETIME>(),
        ),
    ]
}

fn binding(
    ordinal: usize,
    value: usize,
    length: usize,
    status: usize,
    kind: u16,
    max_length: usize,
) -> DBBINDING {
    DBBINDING {
        iOrdinal: ordinal,
        obValue: value,
        obLength: length,
        obStatus: status,
        dwPart: (DBPART_VALUE.0 | DBPART_LENGTH.0 | DBPART_STATUS.0) as u32,
        dwMemOwner: DBMEMOWNER_CLIENTOWNED.0 as u32,
        eParamIO: DBPARAMIO_NOTPARAM.0 as u32,
        cbMaxLen: max_length,
        wType: kind,
        ..Default::default()
    }
}

/// One row of [`super::SELECT_COLUMNS`], laid out for a single accessor.
///
/// `repr(C)` because the offsets handed to `CreateAccessor` are this type's, and
/// a reordering compiler would leave the provider writing into the wrong field.
#[repr(C)]
#[derive(Clone, Copy)]
struct Row {
    path: [u16; PATH_UNITS],
    path_status: u32,
    path_length: u32,
    name: [u16; NAME_UNITS],
    name_status: u32,
    name_length: u32,
    item_type: [u16; TYPE_UNITS],
    item_type_status: u32,
    item_type_length: u32,
    attributes: u32,
    attributes_status: u32,
    attributes_length: u32,
    modified: FILETIME,
    modified_status: u32,
    modified_length: u32,
}

impl Row {
    const EMPTY: Self = Self {
        path: [0; PATH_UNITS],
        path_status: 0,
        path_length: 0,
        name: [0; NAME_UNITS],
        name_status: 0,
        name_length: 0,
        item_type: [0; TYPE_UNITS],
        item_type_status: 0,
        item_type_length: 0,
        attributes: 0,
        attributes_status: 0,
        attributes_length: 0,
        modified: FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        },
        modified_status: 0,
        modified_length: 0,
    };

    /// This row as a hit, or `None` when it carries no usable path.
    ///
    /// A row without a path is dropped rather than reported: the path is what the
    /// launcher opens, and a hit that cannot be opened is worse than one that is
    /// missing. Everything else degrades -- the name falls back to the path's last
    /// component, the kind to the item type, the timestamp to `None`.
    fn hit(&self) -> Option<FileHit> {
        let path = OsString::from_wide(text(&self.path, self.path_status, self.path_length)?);
        let path = PlatformPath::new(path);

        let name = match text(&self.name, self.name_status, self.name_length) {
            // Lossy on purpose: a name is displayed and scored, never used as
            // identity -- that is the path's job -- so a file the catalog spells
            // in unpaired surrogates still appears in the launcher.
            Some(units) => OsString::from_wide(units).to_string_lossy().into_owned(),
            None => path
                .as_path()
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        };

        let item_type = text(&self.item_type, self.item_type_status, self.item_type_length)
            .map(String::from_utf16_lossy)
            .unwrap_or_default();
        let kind = if self.attributes_status == DBSTATUS_S_OK.0 as u32 {
            if self.attributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
                FileKind::File
            } else {
                FileKind::Directory
            }
        } else if item_type.eq_ignore_ascii_case("Directory") {
            // The documented value `System.ItemType` carries for a folder. Used
            // only when the attribute mask is missing, because a string compare
            // is a weaker test than a bit test.
            FileKind::Directory
        } else {
            FileKind::File
        };

        let modified = if self.modified_status == DBSTATUS_S_OK.0 as u32 {
            let ticks =
                (u64::from(self.modified.dwHighDateTime) << 32) | u64::from(self.modified.dwLowDateTime);
            super::unix_seconds_from_file_time(ticks)
        } else {
            None
        };

        Some(FileHit {
            name,
            path,
            kind,
            modified_unix_seconds: modified,
        })
    }
}

/// The code units a bound `DBTYPE_WSTR` column carries, or `None` when the row
/// has nothing usable there.
///
/// Only `DBSTATUS_S_OK` is accepted. A truncated status means the provider had
/// more to say than the buffer could hold, and a half a path is not a path.
fn text(buffer: &[u16], status: u32, length: u32) -> Option<&[u16]> {
    if status != DBSTATUS_S_OK.0 as u32 {
        return None;
    }
    // `obLength` counts bytes for a string column, not characters.
    let units = (length as usize / size_of::<u16>()).min(buffer.len());
    if units == 0 {
        return None;
    }
    Some(&buffer[..units])
}

/// A string the COM task allocator owns and this frame must free.
struct CoString(PWSTR);

impl Drop for CoString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: frees exactly what the provider allocated, once.
            unsafe { CoTaskMemFree(Some(self.0.as_ptr().cast())) };
        }
    }
}

/// An OLE DB accessor released when this frame ends, however it ends.
struct Accessor<'a> {
    factory: &'a IAccessor,
    handle: HACCESSOR,
}

impl Drop for Accessor<'_> {
    fn drop(&mut self) {
        // SAFETY: releases the accessor this guard was handed, once; the
        // reference count is not wanted back.
        let _ = unsafe { self.factory.ReleaseAccessor(self.handle, None) };
    }
}
