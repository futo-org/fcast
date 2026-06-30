#[cfg(feature = "__schema")]
fn main() {
    use askama::Template;
    use fcast_protocol::VersionMessage;

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

    fn strip_top_attribs(input: &str) -> String {
        input
            .lines()
            .filter(|line| !line.starts_with("#["))
            .map(|line| line.replace("pub ", ""))
            .collect::<Vec<_>>()
            .join("\n")
    }

    // stringify type
    macro_rules! st {
        ($typ:ident) => {
            strip_top_attribs($typ::type_string().trim_end_matches('\n'))
        };
    }

    // json schema template
    macro_rules! jt {
        ($typ:ident) => {
            RustTypeTemplate {
                rust_type: st!($typ),
                // schema_definition: serde_json::to_string_pretty(&schema_for!($typ)).unwrap(),
            }
        };
    }

    let doc = V4DocumentationTemplate {
        version_message: jt!(VersionMessage),
        flatbuffer_source: include_str!("../flatbuffers/fcast.fbs").to_owned(),
    };

    std::fs::write(
        "../../docs/docs/protocol/v4.md",
        doc.render().unwrap(),
    )
    .unwrap();
}
