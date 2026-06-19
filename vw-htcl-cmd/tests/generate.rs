// Integration tests: parse synthetic and real man pages, then prove the
// generated htcl re-parses cleanly through `vw_htcl` (the same parser
// `vw check` and the LSP use).

use std::path::Path;

use vw_htcl_cmd::{generate, parse_man_page, ArgKind, GenerateOptions};

/// A man page exercising every argument shape: required value flag,
/// optional value flag, boolean flag, a multi-word placeholder,
/// required positional, optional positional, and a `Note:` block that
/// must fold into the preceding argument.
const SAMPLE: &str = "
Description:

  Creates a thing. Pass it a list like {a b c} and it just works.

  Returns the created thing, or an error if it fails.

Arguments:

  -period <arg> - (Required) The period, must be > 0.

  -name <arg> - (Optional) The name of the thing.

  -waveform <arg1 arg2 ...> - (Optional) Edge times.

  -add - (Optional) Add instead of replace.

  -quiet - (Optional) Execute quietly.

  Note: errors on the command line are still returned.

  <objects> - (Required) The source objects.

Examples:

  make_thing -period 10

See Also:

   *  destroy_thing
   *  get_things
";

fn assert_reparses(htcl: &str) {
    let parsed = vw_htcl::parse(htcl);
    assert!(
        parsed.errors.is_empty(),
        "generated htcl failed to parse: {:#?}\n---\n{htcl}",
        parsed.errors
    );
}

#[test]
fn parses_every_argument_shape() {
    let page = parse_man_page("make_thing", SAMPLE);

    assert_eq!(page.name, "make_thing");
    assert_eq!(page.see_also, vec!["destroy_thing", "get_things"]);

    let by_ident = |id: &str| {
        page.arguments
            .iter()
            .find(|a| a.ident == id)
            .unwrap_or_else(|| panic!("missing arg {id}"))
    };

    let period = by_ident("period");
    assert_eq!(period.kind, ArgKind::Value);
    assert!(period.required);

    let name = by_ident("name");
    assert_eq!(name.kind, ArgKind::Value);
    assert!(!name.required);

    let waveform = by_ident("waveform");
    assert_eq!(waveform.kind, ArgKind::Value, "multi-word placeholder");

    let add = by_ident("add");
    assert_eq!(add.kind, ArgKind::Boolean);

    // The `Note:` block must have folded into -quiet's description.
    let quiet = by_ident("quiet");
    assert!(
        quiet
            .description
            .iter()
            .any(|l| l.contains("still returned")),
        "Note block did not fold into -quiet: {:?}",
        quiet.description
    );

    let objects = by_ident("objects");
    assert_eq!(objects.kind, ArgKind::Positional);
    assert!(objects.required);
    // A positional was documented, so none is synthesized.
    assert!(page.arguments.iter().all(|a| !a.synthesized));
}

#[test]
fn generated_wrapper_reparses() {
    let page = parse_man_page("make_thing", SAMPLE);
    let htcl = generate(&page, &GenerateOptions::default());

    // Doc braces are neutralized so the arg-list brace match survives.
    assert!(
        htcl.contains("{a b c}".replace('{', "(").replace('}', ")").as_str())
    );
    // Natural name + guarded rename + forward.
    assert!(htcl.contains("proc make_thing {"));
    assert!(htcl.contains("rename make_thing __viv_make_thing"));
    assert!(htcl.contains("lappend cmd -period $period"));
    assert!(htcl.contains("if {$add} { lappend cmd -add }"));
    assert!(htcl.contains("lappend cmd {*}$objects"));

    assert_reparses(&htcl);
}

#[test]
fn synthesizes_operand_when_no_positional() {
    let page = parse_man_page(
        "current_thing",
        "\nDescription:\n\n  Gets the current thing.\n\nArguments:\n\n  \
         -quiet - (Optional) Quietly.\n",
    );
    let synth: Vec<_> =
        page.arguments.iter().filter(|a| a.synthesized).collect();
    assert_eq!(synth.len(), 1, "exactly one synthesized operand");
    assert_eq!(synth[0].kind, ArgKind::Positional);
    assert!(!synth[0].required);

    assert_reparses(&generate(&page, &GenerateOptions::default()));
}

#[test]
fn empty_man_page_still_generates_valid_wrapper() {
    // No Description, no Arguments — the generator must still emit a
    // parseable, self-contained wrapper.
    let page = parse_man_page("noop", "");
    let htcl = generate(&page, &GenerateOptions::default());
    assert!(htcl.contains("proc noop {"));
    assert_reparses(&htcl);
}

/// Smoke test over the real Vivado man pages when a local install is
/// present: every page must generate htcl that re-parses cleanly.
#[test]
fn real_man_pages_reparse() {
    let dir = "/home/ry/Xilinx/2025.1/Vivado/doc/eng/man";
    if !Path::new(dir).exists() {
        eprintln!("skipping: {dir} not present");
        return;
    }
    let mut checked = 0;
    let mut failures = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(dir)];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let stem = match path.file_name().and_then(|s| s.to_str()) {
                Some(s)
                    if s.chars().all(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'
                    }) =>
                {
                    s
                }
                _ => continue, // skip tmp.* / *_Copy junk
            };
            let text = std::fs::read_to_string(&path).unwrap();
            let page = parse_man_page(stem, &text);
            let htcl = generate(&page, &GenerateOptions::default());
            let parsed = vw_htcl::parse(&htcl);
            if !parsed.errors.is_empty() {
                failures.push(format!("{stem}: {:?}", parsed.errors));
            }
            checked += 1;
        }
    }
    eprintln!("checked {checked} real man pages");
    assert!(checked > 500, "expected many man pages, saw {checked}");
    assert!(
        failures.is_empty(),
        "{} man pages produced unparseable htcl:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
