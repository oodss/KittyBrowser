//! `kb ctl`: give one command to the running kb and print its answer.

use crate::commands::{self, COMMANDS};
use crate::control::{self, Output};
use crate::term::printable;
use serde_json::json;
use std::path::{Path, PathBuf};

fn usage() -> String {
    let mut text = String::from(
        "usage: kb ctl <command> [arguments]\n\n\
         Controls the kb browser running in a terminal pane: what you do here\n\
         happens on the page the user is looking at. With several kbs running,\n\
         kb ctl -i N <command> picks one (kb list numbers them); the default is\n\
         the lowest-numbered.\n\n",
    );
    for command in COMMANDS {
        text.push_str(&format!("  {}\n      {}\n", command.synopsis(), command.help));
    }
    text.push_str(
        "\nA <selector> is a CSS selector (the first match is used; it also finds\n\
         elements inside shadow DOM, and a >>> b picks b inside a's shadow root),\n\
         or text=Some words for the element showing that text, or @N for link N\n\
         from kb ctl links, or :focus for whatever has focus.\n\
         kb ctl <command> --help describes one command and its arguments.\n\
         Exit status: 0 on success; 1 if the command failed, with the reason on\n\
         stderr; 2 if the command line was wrong. A command that worked but\n\
         found errors on the page (open, reload, click) still exits 0 and says\n\
         so in its output; in a script, follow it with kb ctl check.",
    );
    text
}

/// Whether an address that isn't an existing file was still meant as one,
/// rather than as a host name or a search.
fn looks_like_a_file(address: &str) -> bool {
    let lower = address.to_ascii_lowercase();
    if address.contains("://") {
        return false;
    }
    if address.starts_with(['.', '/', '~']) {
        return true;
    }
    // "site.com/page.html" and "localhost:8000/page.html" are a host and a
    // path; "pages/about.html" and "about.html" are files.
    let host_like = match address.split_once('/') {
        Some((first, _)) => first.contains(['.', ':']) || first == "localhost",
        None => false,
    };
    !host_like && [".html", ".htm", ".xhtml", ".svg", ".pdf"].iter().any(|ending| lower.ends_with(ending))
}

/// Run `kb ctl` with the words after it. Returns the exit status.
pub fn run(mut words: &[String]) -> i32 {
    // Which kb, when several are running.
    if let [flag, number, rest @ ..] = words {
        if flag == "-i" || flag == "--instance" {
            std::env::set_var("KB_INSTANCE", number);
            words = rest;
        }
    }
    let Some(name) = words.first().filter(|word| !matches!(word.as_str(), "-h" | "--help" | "help")) else {
        println!("{}", usage());
        return 0;
    };
    let Some(command) = commands::find(name) else {
        let names: Vec<&str> = COMMANDS.iter().map(|command| command.name).collect();
        eprintln!("kb ctl: unknown command {name:?}\ncommands: {}\nsee: kb ctl --help", names.join(", "));
        return 2;
    };

    let words = &words[1..];
    // Anywhere, as an agent adds it to a command it was about to run.
    if words.iter().any(|word| word == "--help" || word == "-h") {
        println!("{}", command.describe());
        return 0;
    }
    let mut args = match command.parse(words) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("kb ctl: {}", printable(&error));
            return 2;
        }
    };
    // Where to save an image is ours to deal with, not the browser's.
    let out = args.as_object_mut().and_then(|args| args.remove(commands::OUT));
    let out = out.as_ref().and_then(|out| out.as_str()).map(PathBuf::from);
    // kb's working directory isn't ours, so a relative file path has to be
    // made absolute here.
    if let Some(url) = args["url"].as_str() {
        match std::fs::canonicalize(url) {
            Ok(path) => args["url"] = json!(format!("file://{}", path.display())),
            Err(_) if looks_like_a_file(url) => {
                let path = std::env::current_dir().unwrap_or_default().join(url);
                eprintln!("kb ctl: no such file: {}", printable(&path.display().to_string()));
                return 1;
            }
            Err(_) => {}
        }
    }

    match control::request(command.name, args) {
        Ok(Output::Text(text)) => {
            println!("{}", printable(&text));
            0
        }
        Ok(Output::Image(png)) => {
            let saved = crate::base64::decode(&png)
                .ok_or("kb sent a damaged image".to_owned())
                .and_then(|bytes| save_image(out.as_deref(), &bytes));
            match saved {
                Ok(path) => {
                    println!("{}", printable(&path.display().to_string()));
                    0
                }
                Err(error) => {
                    eprintln!("kb ctl: {}", printable(&error));
                    1
                }
            }
        }
        Err(error) => {
            eprintln!("kb ctl: {}", printable(&error));
            1
        }
    }
}

/// Publish a finished 0600 file in one rename. Replacing an existing
/// destination also replaces its old permissions and never follows a link.
fn save_image(out: Option<&Path>, bytes: &[u8]) -> Result<PathBuf, String> {
    use std::io::Write;
    let folder = match out {
        Some(path) => path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_owned(),
        None => crate::data_dir()?.join("screenshots"),
    };
    let save = || -> std::io::Result<PathBuf> {
        if out.is_none() {
            crate::private_directory(&folder)?;
        } else {
            std::fs::create_dir_all(&folder)?;
        }
        let mut file = tempfile::Builder::new().prefix("kb-screenshot-").suffix(".png").tempfile_in(&folder)?;
        file.write_all(bytes)?;
        match out {
            Some(path) => {
                file.persist(path)?;
                Ok(path.to_owned())
            }
            None => Ok(file.into_temp_path().keep()?),
        }
    };
    save().map_err(|e| format!("saving screenshot in {}: {e}", folder.display()))
}

/// `kb list`: the kbs running now, one per line, numbered. Returns the
/// exit status.
pub fn list() -> i32 {
    let running = control::running();
    if running.is_empty() {
        eprintln!("kb: {}", control::NOT_RUNNING);
        return 1;
    }
    for instance in running {
        std::env::set_var("KB_INSTANCE", instance.to_string());
        let (page, pane, id) = match control::request("status", json!({"listing": true})) {
            Ok(Output::Text(status)) => {
                let field =
                    |name: &str| status.lines().find_map(|line| line.strip_prefix(name)).unwrap_or("").to_owned();
                let url = field("url: ");
                (if url.is_empty() { "about:blank".to_owned() } else { url }, field("pane: "), field("id: "))
            }
            _ => ("(not answering)".to_owned(), String::new(), String::new()),
        };
        // The pane tells kbs apart: an agent knows the one it split. The id
        // names this run of it, so a number taken over later isn't mistaken.
        println!("{}", printable(&format!("{instance}\t{page}\t{pane}\t{id}")));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_replacement_is_private_and_leaves_link_target_alone() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let previous = dir.path().join("previous");
        let out = dir.path().join("out.png");
        std::fs::write(&previous, b"original").unwrap();
        symlink(&previous, &out).unwrap();
        save_image(Some(&out), b"frame").unwrap();
        assert_eq!(std::fs::read(&previous).unwrap(), b"original");
        assert_eq!(std::fs::read(&out).unwrap(), b"frame");
        assert_eq!(out.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn files_and_hosts() {
        assert!(looks_like_a_file("nothere.html"));
        assert!(looks_like_a_file("./site"));
        assert!(looks_like_a_file("/tmp/x"));
        assert!(looks_like_a_file("pages/about.html"));
        assert!(!looks_like_a_file("127.0.0.1:49844/index.html"));
        assert!(!looks_like_a_file("example.com/index.html"));
        assert!(!looks_like_a_file("example.com"));
        assert!(!looks_like_a_file("localhost:3000"));
        assert!(!looks_like_a_file("https://example.com/index.html"));
    }
}
