//! Tool-call ids in the form Anthropic and Bedrock accept.

use std::borrow::Cow;

/// `id` with every character outside `[A-Za-z0-9_-]` replaced by `_`, the only
/// characters Anthropic's `tool_use.id` and Bedrock's `toolUseId` take. A chat
/// can carry another provider's ids (Kimi writes `functions.read:0`), and
/// Claude refuses the whole request over one of them. The same id always maps
/// to the same replacement, so a call and its result still match.
pub(crate) fn portable(id: &str) -> Cow<'_, str> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    if !id.is_empty() && id.chars().all(allowed) {
        return Cow::Borrowed(id);
    }
    if id.is_empty() {
        return Cow::Borrowed("_");
    }
    Cow::Owned(
        id.chars()
            .map(|c| if allowed(c) { c } else { '_' })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::portable;

    #[test]
    fn foreign_ids_keep_their_shape_in_the_accepted_characters() {
        assert_eq!(portable("toolu_01AbC-9"), "toolu_01AbC-9");
        assert_eq!(portable("functions.read:0"), "functions_read_0");
        assert_eq!(portable("call 1/ü"), "call_1__");
        assert_eq!(portable(""), "_");
    }
}
