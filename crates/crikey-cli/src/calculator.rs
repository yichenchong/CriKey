//! The launcher's built-in calculator.
//!
//! Three decisions shape this module:
//!
//! * **It is silent unless it is certain.** Every other provider answers a
//!   query by ranking things it already holds; this one manufactures a row out
//!   of the query text itself, so it has no corpus to be absent from. The only
//!   thing standing between "a calculator" and "a row on every keystroke" is
//!   the parser: a query earns a row exactly when it parses, whole, as an
//!   arithmetic expression with at least one binary operator. `2+2` answers,
//!   `budget.ods` and `2 apples` and `report` do not.
//! * **It is pure and synchronous.** No I/O, no worker thread, no deadline. It
//!   therefore never marks itself pending, which is the only way a source can
//!   leave the view saying "Providers are still responding" forever.
//! * **The row it publishes is not in the catalog.** Like the file provider's
//!   rows, a calculated row cannot be resolved by `SearchService::execute`, so
//!   this module keeps what it just published — generation, item id, and the
//!   text to copy — and the composition root asks it first.

use crikey_core::{Action, ActionId, Category, ExecutionPolicy, Generation, ItemId, PluginId};
use crikey_platform::Clipboard;
use crikey_ui::ResultRow;

/// Owner of the host's own calculated results (spec 10.2 namespacing).
pub(crate) const CALCULATOR_PLUGIN: &str = "builtin.crikey.calculator";

/// The one action a calculated row offers.
const COPY_ACTION_ID: &str = "crikey.calculator.copy";

/// Digits kept when a result is not integral.
///
/// Ten is past the point where a decimal digit tells the user anything and
/// short of the point where binary rounding shows through: `0.1 + 0.2` is
/// `0.30000000000000004` in an `f64`, and ten digits round that back to the
/// `0.3` the user typed. Trailing zeros are trimmed afterwards, so a result
/// that needs fewer digits does not pay for these.
const FRACTION_DIGITS: usize = 10;

/// Why an expression produced no value.
///
/// Every variant is a refusal rather than a guess, and every refusal reaches
/// the user the same way: no row at all. A calculator that answered `1/0` with
/// `inf`, or `2 apples` with `2`, would be worse than one that stayed quiet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CalculationError {
    /// Nothing to evaluate.
    Empty,
    /// A character that is not part of the grammar.
    UnexpectedCharacter,
    /// An operand was expected and something else was found.
    ExpectedOperand,
    /// A `(` with no `)`, or a `)` with no `(`.
    UnbalancedParentheses,
    /// Input remained after a complete expression, as in `2+2 apples`.
    TrailingInput,
    /// A numeric literal that is not a number.
    MalformedNumber,
    /// Division or remainder by zero.
    DivisionByZero,
    /// The result overflowed to infinity, or was not a number.
    NotFinite,
    /// The input parses but computes nothing: a bare number.
    ///
    /// Separate from the other refusals because it is not an error in the
    /// input — see [`evaluate`] for why it is still a refusal.
    NotACalculation,
}

/// Evaluates `query` as an arithmetic expression.
///
/// The grammar, in precedence order from loosest to tightest:
///
/// ```text
/// expression := term (('+' | '-') term)*
/// term       := unary (('*' | '/' | '%') unary)*
/// unary      := ('-' | '+') unary | power
/// power      := atom (('^' | '**') unary)?
/// atom       := number | '(' expression ')'
/// number     := digits ['.' digits] | '.' digits
/// ```
///
/// `power` takes a `unary` on its right and sits *below* `unary` on its left,
/// which is what makes `-2^2` evaluate to `-4` rather than `4` and `2^-3` to
/// `0.125`: exponentiation binds tighter than negation and is
/// right-associative, while `+ - * / %` are left-associative.
///
/// A bare number is refused with [`CalculationError::NotACalculation`]. `42` is
/// not a calculation — it is a query, and quite possibly a query about a file,
/// a version or a port number. Answering it would put a row saying `42` under
/// every numeric query in exchange for telling the user something they just
/// typed. The gate is therefore "at least one binary operator was applied",
/// which also refuses `(42)` and `-42`: parentheses and a sign rearrange a
/// number, they do not compute one.
pub(crate) fn evaluate(query: &str) -> Result<f64, CalculationError> {
    let tokens = tokenize(query)?;
    if tokens.is_empty() {
        return Err(CalculationError::Empty);
    }
    let mut parser = Parser {
        tokens: &tokens,
        position: 0,
        binary_operations: 0,
    };
    let value = parser.expression()?;
    if parser.position != tokens.len() {
        // A `)` that closes nothing lands here rather than in `atom`, which
        // only ever consumes a `)` it opened.
        return Err(match tokens[parser.position] {
            Token::CloseParenthesis => CalculationError::UnbalancedParentheses,
            _ => CalculationError::TrailingInput,
        });
    }
    if parser.binary_operations == 0 {
        return Err(CalculationError::NotACalculation);
    }
    finite(value)
}

/// Renders `value` the way a person writes it.
///
/// One path, not two: rounding to [`FRACTION_DIGITS`] and then stripping the
/// trailing zeros it introduced also strips the whole fractional part of an
/// integral result, so `4` never renders as `4.0` and `0.1 + 0.2` never
/// renders as `0.30000000000000004`. Fixed-point rather than `Display` because
/// `Display` prints an `f64` to the last bit it can distinguish, which is
/// where that rounding noise comes from; neither reaches for exponent
/// notation, so no ordinary magnitude comes back as `1e21`.
pub(crate) fn format_value(value: f64) -> String {
    // `-0.0` would render as `-0`, which is not a number anybody typed an
    // expression to see.
    let value = if value == 0.0 { 0.0 } else { value };
    let rendered = format!("{value:.FRACTION_DIGITS$}");
    rendered.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The built-in calculator: a pure evaluator, the row it last published, and
/// the session's clipboard if it has one.
pub(crate) struct Calculator {
    owner: PluginId,
    /// `None` when this session offers no clipboard, which is not an error
    /// state: the row still renders and selecting it reports why it could not
    /// copy. See [`Calculator::copy`].
    clipboard: Option<Box<dyn Clipboard>>,
    published: Option<PublishedResult>,
}

/// What the last published row resolves to when the user selects it.
struct PublishedResult {
    generation: Generation,
    item: ItemId,
    text: String,
}

impl Calculator {
    pub(crate) fn new(clipboard: Option<Box<dyn Clipboard>>) -> Self {
        Self {
            owner: PluginId(CALCULATOR_PLUGIN.to_owned()),
            clipboard,
            published: None,
        }
    }

    /// The rows this generation's query earns: exactly one, or none at all.
    ///
    /// Never more than one, so the frame this joins can never be the batch
    /// that crosses `max_items_per_plugin_per_query` and gets refused whole.
    pub(crate) fn rows(&mut self, generation: Generation, query: &str) -> Vec<ResultRow> {
        let expression = query.trim();
        let Ok(value) = evaluate(expression) else {
            // Dropped rather than retained: a row the user can no longer see
            // must not stay selectable through a stale item id.
            self.published = None;
            return Vec::new();
        };
        let text = format_value(value);
        let item = ItemId::derived(&self.owner, &Category::Expression, expression);
        let row = ResultRow {
            item: item.clone(),
            label: text.clone(),
            description: expression.to_owned(),
            icon_reference: None,
            icon: None,
            category: Category::Expression.as_str().to_owned(),
            plugin_name: self.owner.0.clone(),
            highlights: Vec::new(),
            argument_hint: None,
            status: None,
            default_action: Some(copy_action()),
            alternate_actions: Vec::new(),
        };
        self.published = Some(PublishedResult {
            generation,
            item,
            text,
        });
        vec![row]
    }

    /// The text behind a selected row, when the selection really is this
    /// module's row published under the generation now on screen.
    pub(crate) fn resolve(&self, generation: Generation, item: &ItemId) -> Option<&str> {
        let published = self.published.as_ref()?;
        (published.generation == generation && &published.item == item).then_some(published.text.as_str())
    }

    /// Copies `text` to the session clipboard.
    ///
    /// The error is a diagnostic for the status line, never a panic. Both
    /// failures it reports are ordinary: a session with no clipboard at all --
    /// a Linux unit with no display server -- and a clipboard that refused the
    /// transfer, which on Windows is another process holding it open and on X11
    /// is a dead connection.
    pub(crate) fn copy(&self, text: &str) -> Result<(), String> {
        let Some(clipboard) = self.clipboard.as_ref() else {
            return Err("this session has no clipboard service".to_owned());
        };
        clipboard.write_text(text).map_err(|error| error.to_string())
    }
}

/// The action a calculated row offers, and the only one it offers.
fn copy_action() -> Action {
    Action {
        action_id: ActionId(COPY_ACTION_ID.to_owned()),
        label: "Copy result".to_owned(),
        description: "Copies the result to the clipboard".to_owned(),
        applicable_categories: vec![Category::Expression],
        icon_reference: None,
        execution_policy: ExecutionPolicy::HostMediated,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Token {
    Number(f64),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    OpenParenthesis,
    CloseParenthesis,
}

fn tokenize(input: &str) -> Result<Vec<Token>, CalculationError> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b' ' | b'\t' => index += 1,
            b'+' => {
                tokens.push(Token::Plus);
                index += 1;
            }
            b'-' => {
                tokens.push(Token::Minus);
                index += 1;
            }
            // `**` is the same operator as `^`, spelled the way a programmer
            // types it; two tokens would make `2**3` parse as `2 * (*3)`.
            b'*' if bytes.get(index + 1) == Some(&b'*') => {
                tokens.push(Token::Caret);
                index += 2;
            }
            b'*' => {
                tokens.push(Token::Star);
                index += 1;
            }
            b'/' => {
                tokens.push(Token::Slash);
                index += 1;
            }
            b'%' => {
                tokens.push(Token::Percent);
                index += 1;
            }
            b'^' => {
                tokens.push(Token::Caret);
                index += 1;
            }
            b'(' => {
                tokens.push(Token::OpenParenthesis);
                index += 1;
            }
            b')' => {
                tokens.push(Token::CloseParenthesis);
                index += 1;
            }
            b'0'..=b'9' | b'.' => {
                let start = index;
                while index < bytes.len() && bytes[index].is_ascii_digit() {
                    index += 1;
                }
                if bytes.get(index) == Some(&b'.') {
                    index += 1;
                    let fraction_start = index;
                    while index < bytes.len() && bytes[index].is_ascii_digit() {
                        index += 1;
                    }
                    // `1.` and a bare `.` are refused: a trailing separator is
                    // an unfinished number, and `budget.` must not evaluate.
                    if index == fraction_start {
                        return Err(CalculationError::MalformedNumber);
                    }
                }
                let literal = &input[start..index];
                let value: f64 = literal.parse().map_err(|_| CalculationError::MalformedNumber)?;
                tokens.push(Token::Number(finite(value)?));
            }
            _ => return Err(CalculationError::UnexpectedCharacter),
        }
    }
    Ok(tokens)
}

/// Refuses a value an `f64` could not represent: an overflowing literal, an
/// overflowing power, or a `0.0 / 0.0` the division guard cannot see.
fn finite(value: f64) -> Result<f64, CalculationError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(CalculationError::NotFinite)
    }
}

struct Parser<'tokens> {
    tokens: &'tokens [Token],
    position: usize,
    /// How many binary operators were applied, which is what separates a
    /// calculation from a number that was merely typed. See [`evaluate`].
    binary_operations: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<Token> {
        self.tokens.get(self.position).copied()
    }

    fn expression(&mut self) -> Result<f64, CalculationError> {
        let mut value = self.term()?;
        while let Some(token @ (Token::Plus | Token::Minus)) = self.peek() {
            self.position += 1;
            self.binary_operations += 1;
            let right = self.term()?;
            value = if token == Token::Plus {
                value + right
            } else {
                value - right
            };
            finite(value)?;
        }
        Ok(value)
    }

    fn term(&mut self) -> Result<f64, CalculationError> {
        let mut value = self.unary()?;
        while let Some(token @ (Token::Star | Token::Slash | Token::Percent)) = self.peek() {
            self.position += 1;
            self.binary_operations += 1;
            let right = self.unary()?;
            value = match token {
                Token::Star => value * right,
                // Refused rather than answered with an infinity: `1/0` has no
                // value, and `inf` is a claim the user did not ask for.
                _ if right == 0.0 => return Err(CalculationError::DivisionByZero),
                Token::Slash => value / right,
                _ => value % right,
            };
            finite(value)?;
        }
        Ok(value)
    }

    fn unary(&mut self) -> Result<f64, CalculationError> {
        match self.peek() {
            Some(Token::Minus) => {
                self.position += 1;
                self.unary().map(|value| -value)
            }
            Some(Token::Plus) => {
                self.position += 1;
                self.unary()
            }
            _ => self.power(),
        }
    }

    fn power(&mut self) -> Result<f64, CalculationError> {
        let base = self.atom()?;
        if self.peek() != Some(Token::Caret) {
            return Ok(base);
        }
        self.position += 1;
        self.binary_operations += 1;
        // A `unary` on the right, so `2^-3` is legal and `2^3^2` groups to the
        // right without a second loop.
        let exponent = self.unary()?;
        finite(base.powf(exponent))
    }

    fn atom(&mut self) -> Result<f64, CalculationError> {
        match self.peek() {
            Some(Token::Number(value)) => {
                self.position += 1;
                Ok(value)
            }
            Some(Token::OpenParenthesis) => {
                self.position += 1;
                let value = self.expression()?;
                if self.peek() != Some(Token::CloseParenthesis) {
                    return Err(CalculationError::UnbalancedParentheses);
                }
                self.position += 1;
                Ok(value)
            }
            _ => Err(CalculationError::ExpectedOperand),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn value(expression: &str) -> f64 {
        evaluate(expression).unwrap_or_else(|error| panic!("`{expression}` must evaluate, got {error:?}"))
    }

    fn error(expression: &str) -> CalculationError {
        evaluate(expression).expect_err(&format!("`{expression}` must be refused"))
    }

    #[test]
    fn multiplication_binds_tighter_than_addition() {
        // The whole point of a parser rather than a left-to-right fold: a fold
        // answers 20 here.
        assert_eq!(value("2+3*4"), 14.0);
        assert_eq!(value("2*3+4"), 10.0);
    }

    #[test]
    fn subtraction_is_left_associative() {
        // Right-associative subtraction answers 9.
        assert_eq!(value("10-3-2"), 5.0);
        // And so is division: right-associative gives 8.
        assert_eq!(value("16/4/2"), 2.0);
    }

    #[test]
    fn exponentiation_is_right_associative_and_binds_tighter_than_negation() {
        // Left-associative gives (2^3)^2 = 64.
        assert_eq!(value("2^3^2"), 512.0);
        assert_eq!(value("2**3**2"), 512.0);
        // Negation applied first would give 4.
        assert_eq!(value("-2^2"), -4.0);
        // The exponent itself may be signed.
        assert_eq!(value("2^-3"), 0.125);
    }

    #[test]
    fn parentheses_override_precedence() {
        assert_eq!(value("(2+3)*4"), 20.0);
        assert_eq!(value("2*(3+4)*(1+1)"), 28.0);
        assert_eq!(value("-(2+3)"), -5.0);
    }

    #[test]
    fn unary_minus_applies_to_operands_and_stacks() {
        assert_eq!(value("-3+5"), 2.0);
        assert_eq!(value("5*-3"), -15.0);
        assert_eq!(value("--4+0"), 4.0);
    }

    #[test]
    fn division_and_remainder_by_zero_are_refused() {
        assert_eq!(error("1/0"), CalculationError::DivisionByZero);
        assert_eq!(error("1/(3-3)"), CalculationError::DivisionByZero);
        assert_eq!(error("7%0"), CalculationError::DivisionByZero);
    }

    #[test]
    fn malformed_input_is_refused() {
        assert_eq!(error("2+"), CalculationError::ExpectedOperand);
        assert_eq!(error("*2"), CalculationError::ExpectedOperand);
        assert_eq!(error("(2+3"), CalculationError::UnbalancedParentheses);
        assert_eq!(error("2+3)"), CalculationError::UnbalancedParentheses);
        assert_eq!(error("2 apples"), CalculationError::UnexpectedCharacter);
        assert_eq!(error("budget.ods"), CalculationError::UnexpectedCharacter);
        assert_eq!(error("2+3."), CalculationError::MalformedNumber);
        assert_eq!(error(""), CalculationError::Empty);
    }

    #[test]
    fn overflow_is_refused_rather_than_reported_as_infinity() {
        assert_eq!(error("9^9999"), CalculationError::NotFinite);
    }

    #[test]
    fn a_bare_number_is_not_a_calculation() {
        assert_eq!(error("42"), CalculationError::NotACalculation);
        assert_eq!(error("(42)"), CalculationError::NotACalculation);
        assert_eq!(error("-42"), CalculationError::NotACalculation);
        assert_eq!(error("3.5"), CalculationError::NotACalculation);
    }

    #[test]
    fn an_integral_result_is_rendered_without_a_fractional_part() {
        assert_eq!(format_value(4.0), "4");
        assert_eq!(format_value(-15.0), "-15");
        assert_eq!(format_value(0.0), "0");
        assert_eq!(format_value(-0.0), "0");
        // A magnitude other languages render as `1e21`; there must be no
        // exponent here.
        assert_eq!(format_value(1e21), "1000000000000000000000");
    }

    #[test]
    fn a_fractional_result_loses_its_rounding_noise() {
        // `0.1 + 0.2` is `0.30000000000000004`.
        assert_eq!(format_value(value("0.1+0.2")), "0.3");
        assert_eq!(format_value(value("1/3")), "0.3333333333");
        assert_eq!(format_value(value("7/2")), "3.5");
    }

    fn calculator() -> Calculator {
        Calculator::new(None)
    }

    #[test]
    fn a_non_expression_query_publishes_no_row() {
        let mut calculator = calculator();
        for query in ["report", "budget.ods", "2 apples", "firefox", "42", ""] {
            assert!(
                calculator.rows(Generation::ZERO, query).is_empty(),
                "`{query}` must not put a row in the result list"
            );
        }
    }

    #[test]
    fn an_expression_publishes_exactly_one_resolvable_row() {
        let mut calculator = calculator();
        let rows = calculator.rows(Generation::ZERO, " 2 + 2 ");
        assert_eq!(rows.len(), 1, "the calculator answers with one row or none");
        let row = &rows[0];
        assert_eq!(row.label, "4");
        assert_eq!(
            row.description, "2 + 2",
            "the echoed expression is the description"
        );
        assert_eq!(row.plugin_name, CALCULATOR_PLUGIN);
        assert_eq!(row.category, Category::Expression.as_str());
        assert_eq!(
            row.default_action
                .as_ref()
                .map(|action| action.action_id.0.as_str()),
            Some(COPY_ACTION_ID)
        );
        assert_eq!(calculator.resolve(Generation::ZERO, &row.item), Some("4"));
    }

    #[test]
    fn a_row_from_a_superseded_generation_does_not_resolve() {
        let mut calculator = calculator();
        let rows = calculator.rows(Generation::ZERO, "2+2");
        let item = rows[0].item.clone();
        assert_eq!(calculator.resolve(Generation::from_raw(1), &item), None);
        // And a query that answers nothing retires the previous answer.
        calculator.rows(Generation::ZERO, "report");
        assert_eq!(calculator.resolve(Generation::ZERO, &item), None);
    }

    #[derive(Default)]
    struct RecordingClipboard {
        written: RefCell<Vec<String>>,
    }

    impl Clipboard for RecordingClipboard {
        fn read_text(&self) -> crikey_core::Result<Option<String>> {
            Ok(self.written.borrow().last().cloned())
        }

        fn write_text(&self, text: &str) -> crikey_core::Result<()> {
            self.written.borrow_mut().push(text.to_owned());
            Ok(())
        }
    }

    #[test]
    fn selecting_a_row_copies_through_the_session_clipboard() {
        let mut calculator = Calculator::new(Some(Box::<RecordingClipboard>::default()));
        let rows = calculator.rows(Generation::ZERO, "6*7");
        let text = calculator
            .resolve(Generation::ZERO, &rows[0].item)
            .expect("the row it just published resolves")
            .to_owned();
        calculator
            .copy(&text)
            .expect("a working clipboard accepts the copy");
        assert_eq!(
            calculator
                .clipboard
                .as_ref()
                .expect("the clipboard is present")
                .read_text()
                .expect("the recording clipboard reads back"),
            Some("42".to_owned())
        );
    }

    #[test]
    fn a_session_without_a_clipboard_refuses_cleanly() {
        let mut calculator = calculator();
        let rows = calculator.rows(Generation::ZERO, "6*7");
        assert_eq!(rows.len(), 1, "the row renders whether or not a clipboard exists");
        let error = calculator
            .copy("42")
            .expect_err("there is no clipboard to copy into");
        assert!(
            error.contains("clipboard"),
            "the diagnostic must name what was missing, got {error:?}"
        );
    }
}
