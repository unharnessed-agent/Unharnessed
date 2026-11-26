//! The image on the user's clipboard, for a message to carry: a terminal
//! pastes text only, so the system's own tool is asked for it
//! (`osascript` on macOS, `wl-paste` or `xclip` elsewhere). On macOS a
//! file copied in Finder comes first, as its path: the clipboard holds its
//! icon as an image beside it.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::message::from_hex;

/// What the clipboard holds of an image.
#[derive(Debug)]
pub enum Clip {
    /// A file copied in Finder: its path.
    File(PathBuf),
    /// An image itself (a screenshot, one copied in a browser): its bytes.
    Image(Vec<u8>),
}

/// The `AppleScript` that asks for a file first (only one the clipboard
/// holds: its text would be taken for a path), then for the image: its
/// lines, one `-e` each. A file comes as `file <path>`, an image in
/// hexadecimal (`«data PNGf89504E47…»`), nothing as nothing.
const ASK: [&str; 11] = [
    "try",
    "repeat with kind in (clipboard info)",
    "if item 1 of kind is «class furl» then return \"file \" & POSIX path of (the clipboard as «class furl»)",
    "end repeat",
    "end try",
    "try",
    "return the clipboard as «class PNGf»",
    "end try",
    "try",
    "return the clipboard as «class JPEG»",
    "end try",
];

/// What a tool wrote, when it ran and did not fail; `None` when it is not
/// there to run.
fn said(program: &str, args: &[&str]) -> Option<Result<Vec<u8>, ()>> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some(if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(())
    })
}

/// The image on the clipboard: a file's, or its bytes.
///
/// # Errors
///
/// A sentence to show: the clipboard holds no image, or nothing here can read it.
pub fn image() -> Result<Clip, String> {
    let none = || "The clipboard holds no image.".to_string();
    if cfg!(target_os = "macos") {
        let args: Vec<&str> = ASK.iter().flat_map(|line| ["-e", line]).collect();
        let Some(written) = said("osascript", &args) else {
            return Err("The clipboard could not be read: osascript is not there.".to_string());
        };
        let written = String::from_utf8_lossy(&written.map_err(|()| none())?).into_owned();
        // A path ends with its line, spaces kept.
        if let Some(path) = written.strip_prefix("file ") {
            let path = path.strip_suffix('\n').unwrap_or(path);
            return Ok(Clip::File(PathBuf::from(path)));
        }
        return from_hex(&written).map(Clip::Image).ok_or_else(none);
    }
    let tools: [(&str, &[&str]); 2] = [
        ("wl-paste", &["--type", "image/png"]),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
        ),
    ];
    let mut ran = false;
    for (program, args) in tools {
        match said(program, args) {
            Some(Ok(bytes)) if !bytes.is_empty() => return Ok(Clip::Image(bytes)),
            Some(_) => ran = true,
            None => {}
        }
    }
    if ran {
        Err(none())
    } else {
        Err("To paste an image, install wl-clipboard or xclip.".to_string())
    }
}
