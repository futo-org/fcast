use std::fs;

use anyhow::{Context, Result};
use askama::Template;
use clap::{Args, Subcommand};

use crate::workspace;

#[derive(Subcommand)]
pub enum ProtocolCommand {
    /// Regenerate `docs/docs/protocol/v4.md` from the `fcast-protocol` sources.
    ExportV4,
    /// Regenerate the vendored `src/v4_generated.rs` from `flatbuffers/fcast.fbs`.
    /// Requires the `flatc` binary on PATH; run after editing the schema.
    RegenFlatbuffers,
}

#[derive(Args)]
pub struct ProtocolArgs {
    #[clap(subcommand)]
    pub cmd: ProtocolCommand,
}

#[derive(Template)]
#[template(path = "v4_rust_code_block.md")]
struct RustTypeTemplate {
    rust_type: String,
}

#[derive(Template)]
#[template(path = "v4_docs.md")]
struct V4DocumentationTemplate {
    version_message: RustTypeTemplate,
    flatbuffer_source: String,
}

/// Filter out attribute lines and the `pub` qualifier, matching the historical
/// `get-type-string-derive` output.
fn strip_top_attribs(input: &str) -> String {
    input
        .lines()
        .filter(|line| !line.starts_with("#["))
        .map(|line| line.replace("pub ", ""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render a type's own source, pretty-printed, the way the old
/// `#[derive(GetTypeString)]` proc-macro used to. It parses `fcast-protocol`'s
/// source rather than depending on the crate, so no schema-only feature has to
/// ship in the published crate.
fn render_type_source(file: &syn::File, ident: &str) -> Result<String> {
    let mut item = file
        .items
        .iter()
        .find(|item| match item {
            syn::Item::Struct(s) => s.ident == ident,
            syn::Item::Enum(e) => e.ident == ident,
            _ => false,
        })
        .cloned()
        .with_context(|| format!("`{ident}` not found in fcast-protocol/src/lib.rs"))?;

    // A derive macro receives the item with its `#[derive(..)]` attributes
    // already stripped by the compiler; do the same so pretty-printing
    // reproduces the historical output regardless of attribute length.
    let attrs = match &mut item {
        syn::Item::Struct(s) => &mut s.attrs,
        syn::Item::Enum(e) => &mut e.attrs,
        _ => unreachable!(),
    };
    attrs.retain(|attr| !attr.path().is_ident("derive"));

    let rendered = prettyplease::unparse(&syn::File {
        shebang: None,
        attrs: Vec::new(),
        items: vec![item],
    });

    Ok(strip_top_attribs(rendered.trim_end_matches('\n')))
}

impl ProtocolArgs {
    pub fn run(self) -> Result<()> {
        match self.cmd {
            ProtocolCommand::ExportV4 => {
                let root = workspace::root_path()?;
                let proto = root.join("crates/fcast-protocol");

                let lib_src = fs::read_to_string(proto.join("src/lib.rs"))
                    .context("reading fcast-protocol/src/lib.rs")?;
                let parsed =
                    syn::parse_file(&lib_src).context("parsing fcast-protocol/src/lib.rs")?;

                let flatbuffer_source = fs::read_to_string(proto.join("flatbuffers/fcast.fbs"))
                    .context("reading fcast-protocol/flatbuffers/fcast.fbs")?;

                let doc = V4DocumentationTemplate {
                    version_message: RustTypeTemplate {
                        rust_type: render_type_source(&parsed, "VersionMessage")?,
                    },
                    flatbuffer_source,
                };

                let out = root.join("docs/docs/protocol/v4.md");
                fs::write(&out, doc.render()?).with_context(|| format!("writing {out}"))?;
                println!("Wrote {out}");

                Ok(())
            }
            ProtocolCommand::RegenFlatbuffers => {
                let root = workspace::root_path()?;
                let proto = root.join("crates/fcast-protocol");
                let fbs = proto.join("flatbuffers/fcast.fbs");
                let out = proto.join("src/v4_generated.rs");

                let tmp = root.join("target/flatc-gen");
                fs::create_dir_all(&tmp)?;
                flatc_rust::run(flatc_rust::Args {
                    inputs: &[fbs.as_std_path()],
                    out_dir: tmp.as_std_path(),
                    ..Default::default()
                })
                .context("running flatc: is the `flatc` binary installed and in PATH?")?;

                let generated = tmp.join("fcast_generated.rs");
                fs::copy(&generated, &out)
                    .with_context(|| format!("copying {generated} -> {out}"))?;
                println!("Wrote {out}");

                Ok(())
            }
        }
    }
}
