//! End-to-end coverage for sixel graphics: a recording carrying a sixel DCS
//! must drive raster pixels into the rendered output.

use std::io::Cursor;

use agg::Config;

const FONT_FAMILY: &str = "JetBrains Mono";

fn config() -> Config {
    Config {
        // Pin the bundled font so the pipeline renders identically regardless
        // of which fonts the host happens to have installed.
        font_dirs: vec![format!("{}/fonts", env!("CARGO_MANIFEST_DIR"))],
        font_family: Some(FONT_FAMILY.to_owned()),
        show_progress_bar: false,
        ..Default::default()
    }
}

fn render(cast: &str) -> Vec<u8> {
    let mut output = Vec::new();
    agg::run(Cursor::new(cast), &mut output, config()).expect("render should succeed");

    output
}

/// Build a one-line v2 asciicast whose middle output event carries `dcs`.
fn cast_with(dcs: &str) -> String {
    format!(
        concat!(
            "{{\"version\": 2, \"width\": 20, \"height\": 6}}\n",
            "[0.0, \"o\", \"before \"]\n",
            "[0.5, \"o\", {dcs}]\n",
            "[1.0, \"o\", \" after\"]\n",
        ),
        dcs = serde_json::to_string(dcs).unwrap()
    )
}

#[test]
fn renders_sixel_dcs_payload() {
    // A 1x6 red sixel between two text fragments.
    let sixel = "\u{1b}Pq#0;2;100;0;0~\u{1b}\\";
    // The same DCS without the mandatory `q` introducer decodes to nothing,
    // yet avt treats both as ignorable device control strings — so the
    // recordings share an identical timeline, cursor path, and text grid.
    // Any difference in the rendered output must come from the sixel raster.
    let inert = "\u{1b}P#0;2;100;0;0~\u{1b}\\";

    let with_image = render(&cast_with(sixel));
    let without_image = render(&cast_with(inert));

    assert!(with_image.starts_with(b"GIF"), "expected a GIF stream");
    assert!(without_image.starts_with(b"GIF"), "expected a GIF stream");
    assert_ne!(
        with_image, without_image,
        "sixel raster should change the rendered frames"
    );
}
