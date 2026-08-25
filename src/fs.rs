//! Read-only filesystem access, over the directories the manifest step
//! declared under `dirs:`.
//!
//! QuickJS has no filesystem of its own, so these are plain globals backed by
//! `std::fs`, which under `wasm32-wasip2` reaches exactly the directories the
//! host preopened — every declared `dirs:` entry, at the path it was declared
//! at, and nothing else. Writes are absent because those preopens are
//! read-only; a script that needs to hand something back makes a canister call
//! with it.
//!
//! ```js
//! for (const name of readDir("assets")) {
//!     const path = joinPath("assets", name);
//!     if (isFile(path)) print(path, fileSize(path));
//! }
//! ```
//!
//! The reads throw with the underlying error when they fail, and the
//! predicates answer `false` rather than throwing, so `exists`/`isFile`/`isDir`
//! can be asked about a path that may be unreachable. What a script cannot tell
//! apart that way is a path that is absent from a directory it may read and one
//! outside every preopen, which is why a failed read explains what the step
//! declared (see [`path_hint`]).

use std::path::Path;

use rquickjs::atom::PredefinedAtom;
use rquickjs::class::{Trace, Tracer};
use rquickjs::function::{Rest, This};
use rquickjs::{Class, Ctx, Function, JsLifetime, Object, Result as JsResult, TypedArray, Value};

use crate::engine::throw;

/// Register the filesystem globals.
pub fn register(ctx: &Ctx<'_>) -> JsResult<()> {
    let globals = ctx.globals();
    globals.set("readFile", Function::new(ctx.clone(), read_file)?)?;
    globals.set(
        "readFileBytes",
        Function::new(ctx.clone(), read_file_bytes)?,
    )?;
    globals.set("readDir", Function::new(ctx.clone(), read_dir)?)?;
    globals.set("exists", Function::new(ctx.clone(), exists)?)?;
    globals.set("isFile", Function::new(ctx.clone(), is_file)?)?;
    globals.set("isDir", Function::new(ctx.clone(), is_dir)?)?;
    globals.set("isSymlink", Function::new(ctx.clone(), is_symlink)?)?;
    globals.set("fileSize", Function::new(ctx.clone(), file_size)?)?;
    globals.set("joinPath", Function::new(ctx.clone(), join_path)?)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// `readFile(path)` — the file's contents as text, throwing if it is not valid
/// UTF-8. [`read_file_bytes`] is the form for a file that is not text.
fn read_file(ctx: Ctx<'_>, path: String) -> JsResult<String> {
    std::fs::read_to_string(&path).map_err(|e| fail(&ctx, "readFile", &path, &e))
}

/// `readFileBytes(path)` — the file's contents as a `Uint8Array`.
fn read_file_bytes<'js>(ctx: Ctx<'js>, path: String) -> JsResult<TypedArray<'js, u8>> {
    let bytes = std::fs::read(&path).map_err(|e| fail(&ctx, "readFileBytes", &path, &e))?;
    TypedArray::new(ctx, bytes)
}

/// `readDir(path)` — the names of the directory's entries, without the
/// directory itself: `joinPath` puts the two back together.
///
/// The entries come back as a [`DirEntries`], the iterator a `for … of` walks
/// and `[...]` spreads. They are read and sorted here rather than yielded as
/// the host hands them over, so that a directory that cannot be read fails at
/// the call rather than partway through the loop, and so that a script walking
/// a tree does the same work in the same order on every run.
fn read_dir<'js>(ctx: Ctx<'js>, path: String) -> JsResult<Class<'js, DirEntries>> {
    let entries = std::fs::read_dir(&path).map_err(|e| fail(&ctx, "readDir", &path, &e))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| fail(&ctx, "readDir", &path, &e))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Class::instance(ctx, DirEntries { names, cursor: 0 })
}

/// What [`read_dir`] returns: the entry names of one directory, in the shape a
/// generator has — a `next()` that yields them one at a time, and a
/// `Symbol.iterator` that hands back the iterator itself. So it is consumed the
/// way any other iterator is, and, like any other iterator, once.
///
/// ```js
/// for (const name of readDir("assets")) …
/// const names = [...readDir("assets")];
/// ```
#[rquickjs::class(rename = "DirEntries")]
pub struct DirEntries {
    names: Vec<String>,
    cursor: usize,
}

/// Holds no JS values, so its GC trace is empty.
impl<'js> Trace<'js> for DirEntries {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

// No `'js`-bound state, so the lifetime brand is the identity. See
// [`crate::principal::Principal`] for why this is written by hand.
unsafe impl<'js> JsLifetime<'js> for DirEntries {
    type Changed<'to> = DirEntries;
}

#[rquickjs::methods]
impl DirEntries {
    /// The iterator protocol's `next()`: `{ value, done }`, where `value` is
    /// the next entry name and `done` says the directory is exhausted.
    fn next<'js>(&mut self, ctx: Ctx<'js>) -> JsResult<Object<'js>> {
        let step = Object::new(ctx.clone())?;
        match self.names.get(self.cursor) {
            Some(name) => {
                self.cursor += 1;
                step.set(PredefinedAtom::Value, name.as_str())?;
                step.set(PredefinedAtom::Done, false)?;
            }
            None => {
                step.set(PredefinedAtom::Value, Value::new_undefined(ctx))?;
                step.set(PredefinedAtom::Done, true)?;
            }
        }
        Ok(step)
    }

    /// An iterator is its own iterable, so `for … of` and `[...]` reach the
    /// same cursor `next()` does rather than restarting one of their own.
    #[qjs(rename = PredefinedAtom::SymbolIterator)]
    fn iterate<'js>(this: This<Value<'js>>) -> Value<'js> {
        this.0
    }
}

/// `fileSize(path)` — the file's length in bytes. A directory has no length
/// worth reporting, so asking for one is an error rather than a number that
/// means whatever the host filesystem happens to store.
fn file_size(ctx: Ctx<'_>, path: String) -> JsResult<f64> {
    let meta = std::fs::metadata(&path).map_err(|e| fail(&ctx, "fileSize", &path, &e))?;
    if meta.is_dir() {
        return Err(throw(
            &ctx,
            &format!("fileSize('{path}') failed: that path is a directory, not a file"),
        ));
    }
    Ok(meta.len() as f64)
}

// ---------------------------------------------------------------------------
// Predicates. Each follows symlinks, save for `isSymlink`, and each answers
// `false` for a path it cannot stat at all — one that does not exist, or one
// no preopened directory covers.
// ---------------------------------------------------------------------------

/// `exists(path)` — whether anything is there to read.
fn exists(path: String) -> bool {
    Path::new(&path).exists()
}

/// `isFile(path)` — whether the path is there and is a file.
fn is_file(path: String) -> bool {
    Path::new(&path).is_file()
}

/// `isDir(path)` — whether the path is there and is a directory, and so
/// whether `readDir` is the way to read it.
fn is_dir(path: String) -> bool {
    Path::new(&path).is_dir()
}

/// `isSymlink(path)` — whether the path itself is a symbolic link, without
/// following it. The other predicates answer about the link's target, so this
/// is what a walk checks to keep a link out of the tree it descends.
fn is_symlink(path: String) -> bool {
    std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink())
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// `joinPath(...parts)` — the parts as one path, separated by single slashes
/// however the parts themselves are punctuated. Empty parts drop out, and a
/// part that starts at the root replaces what came before it, as pushing onto
/// a path does.
fn join_path(parts: Rest<String>) -> String {
    let mut joined = String::new();
    for part in parts.iter().filter(|p| !p.is_empty()) {
        if part.starts_with('/') {
            joined.clear();
        } else if !joined.is_empty() && !joined.ends_with('/') {
            joined.push('/');
        }
        joined.push_str(part);
    }
    joined
}

/// The path's components, with the separators and the `.`s that name no
/// directory of their own dropped, for comparing one declared path to another.
fn components(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect()
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// The exception a failed operation throws: what was attempted, on what path,
/// why it failed, and — since a plugin reaches only what its step declared —
/// what the step would have to declare for it to work.
fn fail(ctx: &Ctx<'_>, what: &str, path: &str, err: &std::io::Error) -> rquickjs::Error {
    throw(
        ctx,
        &format!("{what}('{path}') failed: {err}{}", path_hint(ctx, path)),
    )
}

/// The advice an unreadable path deserves.
///
/// The filesystem holds the directories the step declared under `dirs:` and
/// nothing else, so a path outside every one of them is the usual reason a read
/// fails — and it fails with a WASI error about preopened descriptors, which
/// says nothing about the manifest the reader would have to fix. A path that
/// names a declared *file* is not on the filesystem at all: the host read it and
/// passed the contents inline, which is what the `files` global holds.
///
/// Read from the globals the way the script sees them, so a script that
/// reassigned them is told about what it made of them.
fn path_hint(ctx: &Ctx<'_>, path: &str) -> String {
    let globals = ctx.globals();

    if let Ok(files) = globals.get::<_, Object<'_>>("files")
        && files
            .get::<_, Option<String>>(path)
            .ok()
            .flatten()
            .is_some()
    {
        return format!(
            "; '{path}' is declared in the step's `files:`, whose contents the host passes \
             inline — read it as files['{path}']"
        );
    }

    let dirs = globals.get::<_, Vec<String>>("dirs").unwrap_or_default();
    let wanted = components(path);
    if dirs.iter().any(|dir| wanted.starts_with(&components(dir))) {
        return String::new();
    }
    match dirs.len() {
        0 => "; the step declares no `dirs:`, so no path is readable".to_string(),
        _ => format!(
            "; the step's `dirs:` declares {}, and a path outside those is not readable",
            dirs.iter()
                .map(|d| format!("'{d}'"))
                .collect::<Vec<_>>()
                .join(", "),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::testing;

    /// A tree to read, removed when the test that made it ends.
    struct TempTree(PathBuf);

    impl TempTree {
        /// `dir/file.txt` holding "hello", `dir/sub/` holding nothing.
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "icp-js-plugin-fs-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::create_dir_all(path.join("sub")).unwrap();
            std::fs::write(path.join("file.txt"), "hello").unwrap();
            Self(path)
        }

        /// The tree's root as a JavaScript string literal.
        fn root(&self) -> String {
            format!("'{}'", self.0.display().to_string().replace('\\', "\\\\"))
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Run `[description, condition]` assertions with `root` bound to the
    /// tree's root and `path(..)` joining onto it.
    fn assert_tree(checks: &[(&str, &str)]) {
        let tree = TempTree::new();
        let preamble = format!(
            "const root = {};\nconst path = (...parts) => joinPath(root, ...parts);\n",
            tree.root(),
        );
        testing::eval(&(preamble + &testing::assertions(checks))).unwrap();
    }

    #[test]
    fn files_are_read_as_text_and_as_bytes() {
        assert_tree(&[
            ("readFile", "readFile(path('file.txt')) === 'hello'"),
            (
                "readFileBytes",
                "decodeUtf8(readFileBytes(path('file.txt'))) === 'hello'",
            ),
            (
                "byte length",
                "readFileBytes(path('file.txt')).length === 5",
            ),
        ]);
    }

    #[test]
    fn a_directory_enumerates_to_its_sorted_entry_names() {
        assert_tree(&[
            (
                "spread",
                "JSON.stringify([...readDir(root)]) === '[\"file.txt\",\"sub\"]'",
            ),
            ("Array.from", "Array.from(readDir(root)).length === 2"),
            ("empty directory", "[...readDir(path('sub'))].length === 0"),
            (
                "for … of",
                "(() => { let seen = []; for (const name of readDir(root)) seen.push(name); \
                 return seen.join() === 'file.txt,sub'; })()",
            ),
            (
                "entries are names, not paths",
                "readFile(joinPath(root, [...readDir(root)][0])) === 'hello'",
            ),
        ]);
    }

    #[test]
    fn the_entries_are_an_iterator_that_is_its_own_iterable() {
        assert_tree(&[
            ("next", "readDir(root).next().value === 'file.txt'"),
            ("done", "readDir(path('sub')).next().done === true"),
            (
                "exhausted value",
                "readDir(path('sub')).next().value === undefined",
            ),
            (
                "one cursor",
                "(() => { const it = readDir(root); it.next(); return [...it].join() === 'sub'; })()",
            ),
            (
                "Symbol.iterator",
                "(() => { const it = readDir(root); return it[Symbol.iterator]() === it; })()",
            ),
        ]);
    }

    #[test]
    fn predicates_answer_for_what_is_there_and_what_is_not() {
        assert_tree(&[
            ("exists file", "exists(path('file.txt'))"),
            ("exists dir", "exists(root) && exists(path('sub'))"),
            ("exists missing", "!exists(path('nope'))"),
            ("isFile", "isFile(path('file.txt')) && !isFile(path('sub'))"),
            ("isDir", "isDir(path('sub')) && !isDir(path('file.txt'))"),
            ("isFile missing", "!isFile(path('nope'))"),
            ("isDir missing", "!isDir(path('nope'))"),
            ("isSymlink", "!isSymlink(path('file.txt'))"),
            ("fileSize", "fileSize(path('file.txt')) === 5"),
        ]);
    }

    #[test]
    fn a_directory_has_no_file_size() {
        let tree = TempTree::new();
        let reported = testing::error(&format!("fileSize({});", tree.root()));
        assert!(
            reported.contains("is a directory, not a file"),
            "{reported}"
        );
    }

    #[test]
    fn parts_join_into_one_path() {
        testing::assert_script(&[
            (
                "parts",
                "joinPath('assets', 'img', 'a.png') === 'assets/img/a.png'",
            ),
            (
                "punctuation",
                "joinPath('assets/', 'img/') === 'assets/img/'",
            ),
            ("empty parts", "joinPath('', 'assets', '') === 'assets'"),
            ("no parts", "joinPath() === ''"),
            ("one part", "joinPath('assets') === 'assets'"),
            // A part naming the root is where the path starts, as pushing is.
            (
                "absolute part",
                "joinPath('assets', '/etc', 'x') === '/etc/x'",
            ),
        ]);
    }

    /// The WASI error a path outside every preopen fails with names file
    /// descriptors, not the manifest, so the plugin says which paths the step
    /// made readable.
    #[test]
    fn an_undeclared_path_is_reported_against_what_the_step_declared() {
        let reported = testing::error("readFile('elsewhere/data.json');");
        assert!(
            reported.contains("readFile('elsewhere/data.json') failed"),
            "{reported}"
        );
        assert!(reported.contains("declares no `dirs:`"), "{reported}");
    }

    #[test]
    fn a_declared_file_is_reported_as_one_the_host_passed_inline() {
        let mut input = testing::input("readFile('seed.json');");
        input.files.push(crate::FileInput {
            key: None,
            name: "seed.json".into(),
            content: "{}".into(),
        });
        let reported = crate::engine::run(input).unwrap_err();
        assert!(reported.contains("files['seed.json']"), "{reported}");
    }

    #[test]
    fn a_path_under_a_declared_dir_is_reported_as_it_failed() {
        let mut input = testing::input("readDir('assets/missing');");
        input.dirs.push(crate::DirInput {
            key: None,
            path: "assets".into(),
        });
        let reported = crate::engine::run(input).unwrap_err();
        assert!(
            reported.contains("readDir('assets/missing') failed"),
            "{reported}"
        );
        // The step declared the tree the path sits in, so there is no manifest
        // advice to give — only the failure itself.
        assert!(!reported.contains("`dirs:`"), "{reported}");
    }
}
