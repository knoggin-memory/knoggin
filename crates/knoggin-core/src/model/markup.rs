// SPDX-License-Identifier: AGPL-3.0-only
//! The tool-markup detector. Ported with its tests from recalld `src/mcp/args.rs`
//! (lines 440 to 690, tests from line 1095).
//!
//! Agents sometimes leak their own tool-call markup into the text they save. Such a
//! save is refused and stores nothing.

use crate::error::EngineError;

use super::limits::truncate_on_char_boundary;

// The literals below are assembled with `concat!` rather than written out in one
// piece. Emitting the reserved vendor prefix verbatim inside a tool call is exactly
// the corruption this detector defends against, and it damages files written
// through such a call.

/// Reserved client vendor-namespace prefix in an opening tag.
const VENDOR_OPEN: &str = concat!("<", "ant", "ml", ":");
/// Reserved client vendor-namespace prefix in a closing tag.
const VENDOR_CLOSE: &str = concat!("</", "ant", "ml", ":");

/// R1 anchors. The prefix is reserved to the client's serializer, so one
/// occurrence is decisive.
const VENDOR_LITERALS: [&str; 2] = [VENDOR_OPEN, VENDOR_CLOSE];

/// R2 anchors. `function_calls` is anchored in both spellings; `invoke` and
/// `parameter` only in the vendor-prefixed form, because the bare
/// `<parameter name=` is common in TestNG and Maven XML.
const INVOKE_LITERALS: [&str; 6] = [
    "<function_calls>",
    "</function_calls>",
    concat!("<", "ant", "ml", ":", "function_calls>"),
    concat!("</", "ant", "ml", ":", "function_calls>"),
    concat!("<", "ant", "ml", ":", "invoke name="),
    concat!("<", "ant", "ml", ":", "parameter name="),
];

/// R3 anchors: exact tags of the two camelCase parameter names, which no real
/// XML schema spells that way.
const DISTINCTIVE_PARAM_TOKENS: [&str; 4] =
    ["</fullText>", "<fullText>", "</parentId>", "<parentId>"];

/// Parameter names that may take part in an R4 adjacency.
const PARAM_TAGS: [&str; 10] = [
    "emotions",
    "entities",
    "fullText",
    "memories",
    "namespace",
    "parentId",
    "summary",
    "supersedes",
    "tags",
    "topics",
];

/// Terminators that may follow a parameter close tag (R4).
const FOLLOWER_TERMINATORS: [&str; 3] = ["</invoke>", "</parameter>", "</function_calls>"];

/// Terminators that may end a text field (R5). `</parameter>` is left out: a
/// DocBook note may end in one.
const TRAILING_TERMINATORS: [&str; 2] = ["</invoke>", "</function_calls>"];

/// White space allowed between an R4 close tag and its follower.
const WS: [char; 4] = [' ', '\t', '\n', '\r'];

/// Longest window searched for a tag name after `</`, which keeps R4 linear on
/// adversarial input.
const TAG_NAME_SCAN_LIMIT: usize = 32;

/// Which detection rule fired. Evaluation order is R2, R1, R3, R4, R5: every
/// vendor-prefixed invoke literal also contains a vendor literal, and the longer
/// token is the more useful one to report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkupRule {
    /// R1: the reserved client vendor-namespace prefix in tag position.
    VendorNamespace,
    /// R2: a `function_calls` literal, or a vendor-prefixed `<invoke name=` / `<parameter name=`.
    InvokeAttribute,
    /// R3: an exact open or close tag of a camelCase parameter name.
    DistinctiveParamTag,
    /// R4: a parameter close tag directly followed by another parameter tag or a terminator.
    TagAdjacency,
    /// R5: the text ends with a call terminator.
    TrailingTerminator,
}

/// Where and why the detector fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkupFinding {
    /// The rule that matched.
    pub rule: MarkupRule,
    /// The literal that matched; a fixed token, never user text.
    pub token: String,
    /// Byte offset of `token` in the scanned text.
    pub offset: usize,
}

/// Advice appended to every markup rejection.
const MARKUP_ADVICE: &str = "Arguments appear to have been serialized incorrectly; nothing was \
     stored. Re-send with structured JSON arguments and no XML tool-call tags in text fields.";

/// The earliest occurrence of any literal; ties go to the longer literal.
fn first_literal_match(text: &str, literals: &[&'static str]) -> Option<(usize, &'static str)> {
    literals
        .iter()
        .filter_map(|lit| text.find(lit).map(|at| (at, *lit)))
        .min_by(|a, b| a.0.cmp(&b.0).then(b.1.len().cmp(&a.1.len())))
}

fn scan_literals(text: &str, literals: &[&'static str], rule: MarkupRule) -> Option<MarkupFinding> {
    first_literal_match(text, literals).map(|(offset, token)| MarkupFinding {
        rule,
        token: token.to_owned(),
        offset,
    })
}

/// `s` begins with an opening tag whose name is a parameter name.
fn starts_with_param_open(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('<') else {
        return false;
    };
    PARAM_TAGS.iter().any(|tag| {
        rest.strip_prefix(tag)
            .is_some_and(|after| after.starts_with('>') || after.starts_with(' '))
    })
}

/// R4: a `</param>` close tag followed, across white space only, by another
/// parameter tag or a call terminator.
fn scan_tag_adjacency(text: &str) -> Option<MarkupFinding> {
    for (start, _) in text.match_indices("</") {
        let Some(rest) = text.get(start + 2..) else {
            continue;
        };
        let window = truncate_on_char_boundary(rest, TAG_NAME_SCAN_LIMIT);
        let Some(gt) = window.find('>') else {
            continue;
        };
        let name = &window[..gt];
        if !PARAM_TAGS.contains(&name) {
            continue;
        }
        let follower = rest[gt + 1..].trim_start_matches(WS);
        let adjacent = FOLLOWER_TERMINATORS.iter().any(|t| follower.starts_with(t))
            || starts_with_param_open(follower);
        if adjacent {
            return Some(MarkupFinding {
                rule: MarkupRule::TagAdjacency,
                token: format!("</{name}>"),
                offset: start,
            });
        }
    }
    None
}

/// R5: the text ends with a call terminator. This rule carries the most false
/// positives (a note ending in a quoted BPEL terminator) and is accepted anyway.
fn scan_trailing_terminator(text: &str) -> Option<MarkupFinding> {
    let trimmed = text.trim_end();
    TRAILING_TERMINATORS
        .iter()
        .find(|t| trimmed.ends_with(**t))
        .map(|t| MarkupFinding {
            rule: MarkupRule::TrailingTerminator,
            token: (*t).to_owned(),
            offset: trimmed.len() - t.len(),
        })
}

/// Scans `text` for literal client tool-call markup. Linear in the length of `text`.
pub fn scan_client_markup(text: &str) -> Option<MarkupFinding> {
    scan_literals(text, &INVOKE_LITERALS, MarkupRule::InvokeAttribute)
        .or_else(|| scan_literals(text, &VENDOR_LITERALS, MarkupRule::VendorNamespace))
        .or_else(|| {
            scan_literals(
                text,
                &DISTINCTIVE_PARAM_TOKENS,
                MarkupRule::DistinctiveParamTag,
            )
        })
        .or_else(|| scan_tag_adjacency(text))
        .or_else(|| scan_trailing_terminator(text))
}

/// Refuses `text` if it contains literal client tool-call markup. The whole save
/// is refused rather than cleaned: the fields that never arrived cannot be recovered.
pub fn reject_client_markup(field: &'static str, text: &str) -> Result<(), EngineError> {
    match scan_client_markup(text) {
        None => Ok(()),
        Some(f) => Err(EngineError::invalid(
            field,
            format!(
                "Parameter '{field}' contains literal tool-call markup (\"{}\" at byte offset \
                 {}). {MARKUP_ADVICE}",
                f.token, f.offset
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `text` is rejected by exactly `rule`, with exactly `token` at exactly `offset`.
    fn assert_rejected(text: &str, rule: MarkupRule, token: &str, offset: usize) {
        let found = scan_client_markup(text)
            .unwrap_or_else(|| panic!("expected a rejection, got none for {text:?}"));
        assert_eq!(found.rule, rule, "rule for {text:?}");
        assert_eq!(found.token, token, "token for {text:?}");
        assert_eq!(found.offset, offset, "offset for {text:?}");
        assert_eq!(
            &text[found.offset..found.offset + found.token.len()],
            found.token,
            "offset does not point at the token for {text:?}"
        );
    }

    fn assert_accepted(text: &str) {
        assert_eq!(
            scan_client_markup(text),
            None,
            "false positive on legitimate text: {text:?}"
        );
    }

    /// The corrupted `fullText` tail from recalld's incident report.
    fn report_artifact() -> String {
        "Reviewed the storage layer and wrote up the design notes</fullText>\n\
         <entities>[\"recalld\"]</entities>\n\
         <tags>[\"topic/rust\"]</tags>\n\
         </invoke>\n"
            .to_owned()
    }

    // ── A. Must reject ──────────────────────────────────────────────

    #[test]
    fn a1_report_artifact_tail_is_rejected() {
        let text = report_artifact();
        assert_rejected(
            &text,
            MarkupRule::DistinctiveParamTag,
            "</fullText>",
            text.find("</fullText>").unwrap(),
        );
    }

    #[test]
    fn a2_vendor_prefix_inside_a_sentence_is_rejected() {
        let text = format!("The client spliced {VENDOR_OPEN}invoke tags into my note.");
        assert_rejected(
            &text,
            MarkupRule::VendorNamespace,
            VENDOR_OPEN,
            text.find(VENDOR_OPEN).unwrap(),
        );
    }

    #[test]
    fn a3_vendor_prefixed_parameter_open_tag_is_rejected() {
        let text = format!("{VENDOR_OPEN}parameter>value");
        assert_rejected(&text, MarkupRule::VendorNamespace, VENDOR_OPEN, 0);
    }

    #[test]
    fn a4_vendor_prefixed_invoke_attribute_is_rejected() {
        let literal = concat!("<", "ant", "ml", ":", "invoke name=");
        let text = format!("{literal}\"store_memory\">");
        assert_rejected(&text, MarkupRule::InvokeAttribute, literal, 0);
    }

    #[test]
    fn a5_vendor_prefixed_parameter_attribute_is_rejected() {
        let literal = concat!("<", "ant", "ml", ":", "parameter name=");
        let text = format!("{literal}\"fullText\">");
        assert_rejected(&text, MarkupRule::InvokeAttribute, literal, 0);
    }

    #[test]
    fn a6_vendor_prefixed_function_calls_open_and_close_are_rejected() {
        let open = concat!("<", "ant", "ml", ":", "function_calls>");
        let close = concat!("</", "ant", "ml", ":", "function_calls>");
        assert_rejected(open, MarkupRule::InvokeAttribute, open, 0);
        let text = format!("note body{close}");
        assert_rejected(
            &text,
            MarkupRule::InvokeAttribute,
            close,
            text.find(close).unwrap(),
        );
    }

    #[test]
    fn a6b_bare_function_calls_tags_are_rejected() {
        assert_rejected(
            "<function_calls>",
            MarkupRule::InvokeAttribute,
            "<function_calls>",
            0,
        );
        assert_rejected(
            "body</function_calls>",
            MarkupRule::InvokeAttribute,
            "</function_calls>",
            4,
        );
    }

    #[test]
    fn a6c_bare_attribute_first_invoke_still_falls_to_another_rule_when_corrupt() {
        let text =
            "<invoke name=\"store_memory\">\n<parameter name=\"summary\">x</summary>\n</invoke>";
        assert_rejected(
            text,
            MarkupRule::TagAdjacency,
            "</summary>",
            text.find("</summary>").unwrap(),
        );
    }

    #[test]
    fn a7_bare_full_text_close_tag_is_rejected() {
        assert_rejected(
            "body</fullText>",
            MarkupRule::DistinctiveParamTag,
            "</fullText>",
            4,
        );
    }

    #[test]
    fn a8_bare_parent_id_close_tag_is_rejected() {
        assert_rejected(
            "x</parentId>y",
            MarkupRule::DistinctiveParamTag,
            "</parentId>",
            1,
        );
    }

    #[test]
    fn a8b_bare_camel_case_open_tags_are_rejected() {
        assert_rejected(
            "a<fullText>b",
            MarkupRule::DistinctiveParamTag,
            "<fullText>",
            1,
        );
        assert_rejected(
            "a<parentId>b",
            MarkupRule::DistinctiveParamTag,
            "<parentId>",
            1,
        );
    }

    #[test]
    fn a9_two_generic_param_tags_in_adjacency_are_rejected() {
        assert_rejected(
            "</summary>\n<tags>[]</tags>",
            MarkupRule::TagAdjacency,
            "</summary>",
            0,
        );
    }

    #[test]
    fn a10_param_close_then_whitespace_run_then_terminator_is_rejected() {
        assert_rejected(
            "</tags>\n\n   </invoke>",
            MarkupRule::TagAdjacency,
            "</tags>",
            0,
        );
    }

    #[test]
    fn a11_trailing_terminator_with_trailing_whitespace_is_rejected() {
        let text = "Notes on the storage layer rewrite.\n</invoke>\n\n";
        assert_rejected(
            text,
            MarkupRule::TrailingTerminator,
            "</invoke>",
            text.find("</invoke>").unwrap(),
        );
    }

    #[test]
    fn a12_generic_to_generic_adjacency_with_single_space_is_rejected() {
        assert_rejected(
            "</namespace> <supersedes>",
            MarkupRule::TagAdjacency,
            "</namespace>",
            0,
        );
    }

    #[test]
    fn a13_adjacency_follower_may_carry_attributes() {
        assert_rejected(
            "</entities>\n<summary type=\"text\">x</summary>",
            MarkupRule::TagAdjacency,
            "</entities>",
            0,
        );
    }

    // ── B. Must accept (false-positive proof) ───────────────────────

    #[test]
    fn b1_csharp_doc_comment_is_accepted() {
        assert_accepted(
            "/// <summary>Returns the widget count.</summary>\n/// <returns>int</returns>",
        );
    }

    #[test]
    fn b2_html_details_summary_is_accepted() {
        assert_accepted("<details><summary>Stack trace</summary><pre>panic at...</pre></details>");
    }

    #[test]
    fn b3_atom_entry_is_accepted() {
        assert_accepted("<entry><title>Post</title><summary type=\"text\">Blurb</summary></entry>");
    }

    #[test]
    fn b4_docbook_parameter_element_is_accepted() {
        assert_accepted("The <parameter>timeout</parameter> element controls retry backoff.");
    }

    #[test]
    fn b5_bpel_invoke_mid_prose_is_accepted() {
        assert_accepted(
            "Legacy BPEL wrapped calls in <invoke partnerLink=\"svc\" operation=\"go\"/> \
             ... </invoke> inside a scope, which we replaced with gRPC.",
        );
    }

    #[test]
    fn b6_jsx_fragment_is_accepted() {
        assert_accepted("return <Fragment><TagList tags={tags} /></Fragment>;");
    }

    #[test]
    fn b7_self_referential_prose_about_these_very_parameters_is_accepted() {
        assert_accepted(
            "The MCP store_memory tool takes {\"summary\": \"...\", \"fullText\": \"...\", \
             \"tags\": []}. A client bug spliced parameter markup into fullText, so tags \
             arrived empty.",
        );
    }

    #[test]
    fn b8_xml_config_with_a_namespace_element_is_accepted() {
        assert_accepted("<namespace>prod</namespace>\n<name>ingest-svc</name>");
    }

    #[test]
    fn b9_rust_generics_and_comparisons_are_accepted() {
        assert_accepted("Vec<String>, HashMap<K, V>, impl Iterator<Item = &'a str>");
        assert_accepted("the guard is a < b && c > d, which held under load");
    }

    #[test]
    fn b10_maven_dependency_block_is_accepted() {
        assert_accepted("<dependency><groupId>x</groupId><artifactId>y</artifactId></dependency>");
    }

    #[test]
    fn b11_shell_redirection_is_accepted() {
        assert_accepted("cmd <input.txt >output.txt 2>&1");
    }

    #[test]
    fn b12_adjacency_whose_follower_is_not_a_param_tag_is_accepted() {
        assert_accepted("</summary></details>");
    }

    #[test]
    fn b13_plain_word_usage_is_accepted() {
        assert_accepted(
            "We invoke the parameter validator; the new note supersedes the old summary.",
        );
    }

    #[test]
    fn b14_generic_close_newline_non_param_follower_is_accepted() {
        assert_accepted("<summary>Overview</summary>\n<p>Body text</p>");
    }

    #[test]
    fn b15_empty_and_one_mib_inputs_are_accepted_quickly() {
        assert_accepted("");
        let para = "<p>Lorem ipsum dolor sit <em>amet</em>, consectetur adipiscing elit, \
                    sed do eiusmod tempor incididunt ut labore.</p>\n";
        let mut big = String::with_capacity(1_100_000);
        while big.len() < 1_048_576 {
            big.push_str(para);
        }
        assert!(scan_client_markup(&big).is_none());
    }

    #[test]
    fn b16_param_close_far_from_its_follower_is_accepted() {
        assert_accepted("</summary> and then the <tags> element follows much later");
    }

    #[test]
    fn b17_testng_suite_xml_is_accepted() {
        assert_accepted(
            "<!DOCTYPE suite SYSTEM \"https://testng.org/testng-1.0.dtd\">\n\
             <suite name=\"Regression\" parallel=\"tests\">\n\
             \x20 <parameter name=\"browser\" value=\"chrome\"/>\n\
             \x20 <test name=\"smoke\">\n\
             \x20   <parameter name=\"baseUrl\" value=\"https://staging.example.com\"/>\n\
             \x20   <classes><class name=\"com.example.SmokeTest\"/></classes>\n\
             \x20 </test>\n\
             </suite>",
        );
    }

    #[test]
    fn b18_maven_and_spring_parameter_elements_are_accepted() {
        assert_accepted(
            "The failsafe config sets <parameter name=\"timeout\">30</parameter> and Spring's \
             <bean class=\"Job\"><parameter name=\"retries\">3</parameter></bean> mirrors it.",
        );
    }

    #[test]
    fn b19_bare_invoke_with_a_name_attribute_is_accepted() {
        assert_accepted(
            "The old workflow engine used <invoke name=\"chargeCard\" retries=\"2\"/> for \
             every step, which we replaced with an idempotency key.",
        );
    }

    // ── B'. Documented, deliberately accepted false positives ───────

    #[test]
    fn b_prime_1_note_ending_in_a_quoted_bpel_terminator_is_rejected() {
        let text = "The BPEL scope ended with:\n</invoke>";
        assert_rejected(
            text,
            MarkupRule::TrailingTerminator,
            "</invoke>",
            text.find("</invoke>").unwrap(),
        );
    }

    #[test]
    fn b_prime_2_generic_tags_never_fire_alone() {
        assert_accepted("<summary>x</summary>\n<remarks>y</remarks>");
    }

    // ── D. The rejection message (recalld d6, d6b, d6c) ─────────────

    #[test]
    fn d6_markup_in_full_text_is_rejected_with_the_full_message() {
        let err = reject_client_markup("fullText", &report_artifact()).unwrap_err();
        assert_eq!(err.field(), Some("fullText"));
        let offset = report_artifact().find("</fullText>").unwrap();
        assert_eq!(
            err.to_string(),
            format!(
                "Parameter 'fullText' contains literal tool-call markup \
                 (\"</fullText>\" at byte offset {offset}). {MARKUP_ADVICE}"
            )
        );
        assert!(err.to_string().contains("nothing was stored"));
    }

    #[test]
    fn d6b_markup_in_summary_is_rejected() {
        let err = reject_client_markup("summary", "notes</summary>\n<tags>[]</tags>").unwrap_err();
        assert_eq!(err.field(), Some("summary"));
        assert!(
            err.to_string()
                .starts_with("Parameter 'summary' contains literal tool-call markup")
        );
    }

    #[test]
    fn d6c_legitimate_technical_prose_still_stores() {
        assert!(reject_client_markup("summary", "C# doc comments use <summary> elements").is_ok());
        assert!(
            reject_client_markup(
                "fullText",
                "/// <summary>Returns the widget count.</summary>\n/// <returns>int</returns>\n\
                 The DocBook <parameter>timeout</parameter> element is unrelated."
            )
            .is_ok()
        );
    }
}
