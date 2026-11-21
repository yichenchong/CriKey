// Included in message.rs: nested values share the physical frame DecodeBudget.
typed_message!(ViewExecutionContext, 1024, [], {
    route_id:String=String::new(), logical_view:u64=0, surface:u64=0, revision:u64=0
}, encode(t,o) { put_string(1,&t.route_id,&mut o); put_varint(2,t.logical_view,&mut o); put_varint(3,t.surface,&mut o); put_varint(4,t.revision,&mut o); }, decode(v,f,b) {
    1 => v.route_id=preview_string(f,b,256)?,
    2 => v.logical_view=decode_field_varint(f)?,
    3 => v.surface=decode_field_varint(f)?,
    4 => v.revision=decode_field_varint(f)?,
});
typed_message!(RouteNavigation, 1024, [], {
    kind:u32=0, route_id:String=String::new()
}, encode(t,o) { put_varint(1,u64::from(t.kind),&mut o); put_string(2,&t.route_id,&mut o); }, decode(v,f,b) {
    1 => v.kind=decode_u32(f)?,
    2 => v.route_id=preview_string(f,b,256)?,
});
typed_message!(ViewExecutable, 1024, [], {
    item_id:String=String::new(), action_id:Option<String>=None
}, encode(t,o) { put_string(1,&t.item_id,&mut o); optional_string(&t.action_id,2,&mut o); }, decode(v,f,b) {
    1 => v.item_id=preview_string(f,b,256)?,
    2 => v.action_id=Some(preview_string(f,b,256)?),
});
typed_message!(ViewState, 4096, [], {
    selected_item_id:Option<String>=None, scope_id:Option<String>=None, input:Option<ViewExecutable>=None
}, encode(t,o) { optional_string(&t.selected_item_id,1,&mut o); optional_string(&t.scope_id,2,&mut o); if let Some(x)=&t.input { put_message(3,x,&mut o); } }, decode(v,f,b) {
    1 => v.selected_item_id=Some(preview_string(f,b,256)?),
    2 => v.scope_id=Some(preview_string(f,b,256)?),
    3 => merge_optional(&mut v.input,f,b)?,
});
typed_message!(StructuredViewRequest, 16384, [], {
    route_id:String=String::new(), logical_view:u64=0, surface:u64=0, mode:u32=0,
    previous_revision:Option<u64>=None, state:Option<ViewState>=None, query:String=String::new(),
    anchor_item_id:Option<String>=None, anchor_row_offset:u32=0, reason:u32=0
}, encode(t,o) {
    put_string(1,&t.route_id,&mut o); put_varint(2,t.logical_view,&mut o); put_varint(3,t.surface,&mut o); put_varint(4,u64::from(t.mode),&mut o);
    if let Some(x)=t.previous_revision { put_varint(5,x,&mut o); }
    if let Some(x)=&t.state { put_message(6,x,&mut o); }
    put_string(7,&t.query,&mut o); optional_string(&t.anchor_item_id,8,&mut o); put_varint(9,u64::from(t.anchor_row_offset),&mut o); put_varint(10,u64::from(t.reason),&mut o);
}, decode(v,f,b) {
    1 => v.route_id=preview_string(f,b,256)?,
    2 => v.logical_view=decode_field_varint(f)?,
    3 => v.surface=decode_field_varint(f)?,
    4 => v.mode=decode_u32(f)?,
    5 => v.previous_revision=Some(decode_field_varint(f)?),
    6 => merge_optional(&mut v.state,f,b)?,
    7 => v.query=preview_string(f,b,8192)?,
    8 => v.anchor_item_id=Some(preview_string(f,b,256)?),
    9 => v.anchor_row_offset=decode_u32(f)?,
    10 => v.reason=decode_u32(f)?,
});
typed_message!(ViewMenuEntry, 8388608, [10], {
    kind:u32=0, action_id:Option<String>=None, schema:Option<ArgumentSchema>=None,
    shortcut:Option<String>=None, destructive:bool=false, id:String=String::new(), label:String=String::new(),
    description:String=String::new(), icon_reference:Option<String>=None, entries:Vec<ViewMenuEntry>=Vec::new()
}, encode(t,o) {
    put_varint(1,u64::from(t.kind),&mut o); optional_string(&t.action_id,2,&mut o); if let Some(x)=&t.schema { put_message(3,x,&mut o); }
    optional_string(&t.shortcut,4,&mut o); if t.destructive { put_varint(5,1,&mut o); }
    put_string(6,&t.id,&mut o); put_string(7,&t.label,&mut o); put_string(8,&t.description,&mut o); optional_string(&t.icon_reference,9,&mut o);
    for x in &t.entries { put_message(10,x,&mut o); }
}, decode(v,f,b) {
    1 => v.kind=decode_u32(f)?,
    2 => v.action_id=Some(preview_string(f,b,256)?),
    3 => merge_optional(&mut v.schema,f,b)?,
    4 => v.shortcut=Some(preview_string(f,b,8192)?),
    5 => v.destructive=decode_field_varint(f)?!=0,
    6 => v.id=preview_string(f,b,256)?,
    7 => v.label=preview_string(f,b,8192)?,
    8 => v.description=preview_string(f,b,8192)?,
    9 => v.icon_reference=Some(preview_string(f,b,8192)?),
    10 => bounded_push(&mut v.entries,f,b,256)?,
});
typed_message!(ViewActionSection, 8388608, [3], {
    id:String=String::new(), label:String=String::new(), entries:Vec<ViewMenuEntry>=Vec::new()
}, encode(t,o) { put_string(1,&t.id,&mut o); put_string(2,&t.label,&mut o); for x in &t.entries { put_message(3,x,&mut o); } }, decode(v,f,b) {
    1 => v.id=preview_string(f,b,256)?,
    2 => v.label=preview_string(f,b,8192)?,
    3 => bounded_push(&mut v.entries,f,b,256)?,
});
typed_message!(ViewItemDefinition, 8388608, [4], {
    item:Option<Item>=None, actionless:bool=false, primary_action_id:Option<String>=None, action_sections:Vec<ViewActionSection>=Vec::new()
}, encode(t,o) {
    if let Some(x)=&t.item { put_message(1,x,&mut o); }
    if t.actionless { put_varint(2,1,&mut o); }
    optional_string(&t.primary_action_id,3,&mut o);
    for x in &t.action_sections { put_message(4,x,&mut o); }
}, decode(v,f,b) {
    1 => { preflight_view_item(f.value)?; merge_optional(&mut v.item,f,b)?; },
    2 => v.actionless=decode_field_varint(f)?!=0,
    3 => v.primary_action_id=Some(preview_string(f,b,256)?),
    4 => bounded_push(&mut v.action_sections,f,b,128)?,
});
typed_message!(ViewItemPresentation, 2097152, [], {
    item:Option<ItemPresentation>=None, image:Option<PreviewResponse>=None
}, encode(t,o) {
    if let Some(x)=&t.item {put_message(1,x,&mut o);}
    if let Some(x)=&t.image {put_message(2,x,&mut o);}
}, decode(v,f,b) {
    1 => merge_optional(&mut v.item,f,b)?,
    2 => merge_optional(&mut v.image,f,b)?,
});
typed_message!(ViewCollection, 8388608, [1,2,3], {
    items:Vec<ViewItemPresentation>=Vec::new(), sections:Vec<PresentationDeclaration>=Vec::new(), scopes:Vec<PresentationDeclaration>=Vec::new(),
    initial_selection:Option<String>=None, initial_scope:Option<String>=None
}, encode(t,o) { for x in &t.items { put_message(1,x,&mut o); } for x in &t.sections { put_message(2,x,&mut o); } for x in &t.scopes { put_message(3,x,&mut o); } optional_string(&t.initial_selection,4,&mut o); optional_string(&t.initial_scope,5,&mut o); }, decode(v,f,b) {
    1 => bounded_push(&mut v.items,f,b,4096)?,
    2 => bounded_push(&mut v.sections,f,b,128)?,
    3 => bounded_push(&mut v.scopes,f,b,128)?,
    4 => v.initial_selection=Some(preview_string(f,b,256)?),
    5 => v.initial_scope=Some(preview_string(f,b,256)?),
});
typed_message!(ViewDescriptor, 8388608, [2], {
    title:String=String::new(), definitions:Vec<ViewItemDefinition>=Vec::new(), kind:u32=0, collection:Option<ViewCollection>=None,
    columns:u32=0, detail:Option<PreviewResponse>=None, detail_target:Option<String>=None, form_submit:Option<ViewExecutable>=None
}, encode(t,o) { put_string(1,&t.title,&mut o); for x in &t.definitions { put_message(2,x,&mut o); } put_varint(3,u64::from(t.kind),&mut o); if let Some(x)=&t.collection { put_message(4,x,&mut o); } put_varint(5,u64::from(t.columns),&mut o); if let Some(x)=&t.detail { put_message(6,x,&mut o); } optional_string(&t.detail_target,7,&mut o); if let Some(x)=&t.form_submit { put_message(8,x,&mut o); } }, decode(v,f,b) {
    1 => v.title=preview_string(f,b,8192)?,
    2 => bounded_push(&mut v.definitions,f,b,4096)?,
    3 => v.kind=decode_u32(f)?,
    4 => merge_optional(&mut v.collection,f,b)?,
    5 => v.columns=decode_u32(f)?,
    6 => merge_optional(&mut v.detail,f,b)?,
    7 => v.detail_target=Some(preview_string(f,b,256)?),
    8 => merge_optional(&mut v.form_submit,f,b)?,
});
typed_message!(StructuredViewResponse, 8388608, [], {
    route_id:String=String::new(), logical_view:u64=0, surface:u64=0, status:u32=0, descriptor:Option<Box<ViewDescriptor>>=None
}, encode(t,o) { put_string(1,&t.route_id,&mut o); put_varint(2,t.logical_view,&mut o); put_varint(3,t.surface,&mut o); put_varint(4,u64::from(t.status),&mut o); if let Some(x)=&t.descriptor { put_message(5,x.as_ref(),&mut o); } }, decode(v,f,b) {
    1 => v.route_id=preview_string(f,b,256)?,
    2 => v.logical_view=decode_field_varint(f)?,
    3 => v.surface=decode_field_varint(f)?,
    4 => v.status=decode_u32(f)?,
    5 => { b.charge_box::<ViewDescriptor>()?; v.descriptor=Some(Box::new(nested(f,b)?)); },
});

fn preflight_view_item(bytes: &[u8]) -> Result<(), ProtocolError> {
    let mut cursor = 0;
    let mut counts = [0usize; 16];
    while cursor < bytes.len() {
        let f = read_field(bytes, &mut cursor)?;
        if f.number < 16 {
            counts[f.number as usize] += 1;
        }
        if f.wire_type == WireType::Length && matches!(f.number, 1..=7 | 11 | 12) && f.value.len() > 8192 {
            return Err(typed_error());
        }
        if f.number == 1 && f.value.len() > 256 {
            return Err(typed_error());
        }
        if matches!(f.number, 6 | 9 | 10)
            && counts[f.number as usize] > if f.number == 10 { 256 } else { 128 }
        {
            return Err(typed_error());
        }
        if matches!(f.number, 9 | 10) {
            let mut inner = 0;
            let mut categories = 0;
            while inner < f.value.len() {
                let nested = read_field(f.value, &mut inner)?;
                if nested.wire_type == WireType::Length
                    && ((f.number == 9 && matches!(nested.number, 1 | 2))
                        || (f.number == 10 && matches!(nested.number, 1..=6)))
                    && nested.value.len() > 8192
                {
                    return Err(typed_error());
                }
                if f.number == 10 && nested.number == 1 && nested.value.len() > 256 {
                    return Err(typed_error());
                }
                if f.number == 10 && nested.number == 6 {
                    categories += 1;
                    if categories > 128 {
                        return Err(typed_error());
                    }
                }
            }
        }
    }
    Ok(())
}

// Walk menu bytes without allocating before recursive materialization. Count is
// shared across every section of a target, including submenus.
fn preflight_view_menus(
    bytes: &[u8],
    entry: bool,
    depth: usize,
    count: &mut usize,
) -> Result<(), ProtocolError> {
    if entry {
        *count += 1;
        if depth > 4 || *count > 256 {
            return Err(typed_error());
        }
    }
    let mut cursor = 0;
    while cursor < bytes.len() {
        let f = read_field(bytes, &mut cursor)?;
        if (entry && f.number == 10) || (!entry && f.number == 3) {
            expect_wire(f, WireType::Length)?;
            preflight_view_menus(f.value, true, if entry { depth + 1 } else { 1 }, count)?;
        }
    }
    Ok(())
}
fn preflight_view_definition(bytes: &[u8]) -> Result<(), ProtocolError> {
    let mut cursor = 0;
    let mut count = 0;
    while cursor < bytes.len() {
        let f = read_field(bytes, &mut cursor)?;
        if f.number == 4 {
            expect_wire(f, WireType::Length)?;
            preflight_view_menus(f.value, false, 0, &mut count)?;
        }
    }
    Ok(())
}

impl ViewExecutionContext {
    fn encoded_len(&self) -> usize {
        1 + varint_size(self.route_id.len() as u64)
            + self.route_id.len()
            + 3
            + varint_size(self.logical_view)
            + varint_size(self.surface)
            + varint_size(self.revision)
            + self.unknown.as_bytes().len()
    }
}
