"""The launcher's copies of game files (launcher/crates/, launcher/assets/) stay byte- and value-equal to the originals."""
import ast
import hashlib
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
COPIES = ROOT / "launcher/crates"

# (the game's file or folder, the launcher's copy)
SAME_BYTES = (
    ("crates/sunrise-release-sign/release-keys.txt", "release-verify/keys/sunrise-org-sunrise.txt"),
    ("crates/sunrise-release-sign/native-build-keys.txt", "release-verify/keys/native-builds.txt"),
    ("crates/sunrise-release-sign/tests/signed", "release-verify/tests/signed"),
    ("crates/sunrise-mod-package/tests/index", "smod/tests/index"),
    ("crates/sunrise-theme/tokens.css", "theme/tokens.css"),
    ("packaging/icons/hicolor/16x16/apps/sunrise.png", "../assets/icons/hicolor/16x16/apps/sunrise.png"),
    ("packaging/icons/hicolor/24x24/apps/sunrise.png", "../assets/icons/hicolor/24x24/apps/sunrise.png"),
    ("packaging/icons/hicolor/32x32/apps/sunrise.png", "../assets/icons/hicolor/32x32/apps/sunrise.png"),
    ("packaging/icons/hicolor/48x48/apps/sunrise.png", "../assets/icons/hicolor/48x48/apps/sunrise.png"),
    ("packaging/icons/hicolor/64x64/apps/sunrise.png", "../assets/icons/hicolor/64x64/apps/sunrise.png"),
    ("packaging/icons/hicolor/128x128/apps/sunrise.png", "../assets/icons/hicolor/128x128/apps/sunrise.png"),
    ("packaging/icons/hicolor/256x256/apps/sunrise.png", "../assets/icons/hicolor/256x256/apps/sunrise.png"),
    ("packaging/icons/hicolor/512x512/apps/sunrise.png", "../assets/icons/hicolor/512x512/apps/sunrise.png"),
    ("packaging/icons/hicolor/scalable/apps/sunrise.svg", "../assets/icons/hicolor/scalable/apps/sunrise.svg"),
    ("packaging/icons/sunrise.icns", "../assets/icons/sunrise.icns"),
    ("packaging/icons/sunrise.ico", "../assets/icons/sunrise.ico"),
    ("packaging/linux/sunrise.desktop", "../assets/linux/sunrise.desktop"),
    ("docs/media/village.webp", "../assets/media/village.webp"),
    ("docs/media/underwater.webp", "../assets/media/underwater.webp"),
    ("docs/media/nether.webp", "../assets/media/nether.webp"),
    ("crates/sunrise-mod-package/src/sdk/fixture.tar.xz", "sdk/src/sdk/fixture.tar.xz"),
    ("crates/sunrise-mod-package/src/sdk/bomb.tar.xz", "sdk/src/sdk/bomb.tar.xz"),
    ("crates/sunrise-mod-package/src/sdk/link.tar.xz", "sdk/src/sdk/link.tar.xz"),
    ("crates/sunrise-mod-package/src/sdk/escape.tar.xz", "sdk/src/sdk/escape.tar.xz"),
    ("crates/sunrise-mod-package/src/sdk/inputs.json", "sdk/src/sdk/inputs.json"),
    ("LICENSE", "../assets/LICENSE"),
    ("LICENSES/MIT-Horizon.txt", "../assets/LICENSES/MIT-Horizon.txt"),
    ("crates/sunrise-prepare/tests/support/java_probe.rs", "../tests/support/java_probe.rs"),
    ("crates/sunrise-port-pack/src/zip_intake/source.rs", "../src/zip_intake/source.rs"),
    ("crates/sunrise-port-pack/src/zip_intake/tests.rs", "../src/zip_intake/tests.rs"),
)

# (the game's source, the launcher's copy, the edits that turn one into the other): compared as code, ignoring
# layout, module docs and `use` items, which the launcher's edition formats and orders its own way.
SAME_CODE = (
    ("crates/sunrise-port-pack/src/net.rs", "launcher/src/net.rs", (
        ("pub(crate) fn get_from_hosts", "pub fn get_from_hosts"),
        ('.set("User-Agent", crate::modrinth::AGENT)',
         '.set("User-Agent", concat!("sunrise-launcher/", env!("CARGO_PKG_VERSION")))'),
    )),
    ("crates/sunrise-port-pack/src/zip_intake.rs", "launcher/src/zip_intake.rs", (
        ("""if let Ok(directory) = zip64
            && (directory.entries, directory.size, directory.offset)
                == (classic.entries, classic.size, classic.offset)
        {
            return Ok((directory, Some(at)));
        }""", """if let Ok(directory) = zip64 {
            if (directory.entries, directory.size, directory.offset)
                == (classic.entries, classic.size, classic.offset)
            {
                return Ok((directory, Some(at)));
            }
        }"""),
    )),
)


# (the game's file, a function in it, the SHA-256 of its code, the launcher's function that does the same for show):
# the launcher reads these few things itself, so a change to the game's function is a change to make there too.
MIRRORED = (
    ("crates/sunrise-port-pack/src/import.rs", "prism_archive_layout",
     "9c5580515b41ec63c768d9de42054348e3dec65cc7c04705b0f48cb721929b67", "launcher/src/import.rs prism_instance"),
    ("crates/sunrise-port-pack/src/import.rs", "curseforge_instance_target",
     "59251c22e6e81e67985e2436edfbc7d92f1c5a4b8302f65bae4f901c7d67e2da",
     "launcher/src/other_launchers.rs curseforge_instance_target"),
    ("crates/sunrise-port-pack/src/import.rs", "curseforge",
     "85ff06bf7e97dd51c6d2410587acd3c3d5519fec93e6734b87cef0309003d29c",
     "launcher/src/other_launchers.rs curseforge_instance_target"),
    ("crates/sunrise-port-pack/src/jar.rs", "read_inner",
     "a1fdeb9b3daaf5fbd74cd4e18185beb452759b24e3e2cedabd17595ec5d5850d", "launcher/src/browser/java.rs jar_title"),
    ("crates/sunrise-port-pack/src/jar.rs", "fabric",
     "052665a32cf9980ffcd1d1cb51f72a918e6613d89fb2097562afb306ca4f4fe2", "launcher/src/browser/java.rs jar_title"),
    ("crates/sunrise-port-pack/src/jar.rs", "manifest_value",
     "8eba4a800351645479e1f2fc79c727bf2e3aee7bdcc97b27a3fa249791a2b33e", "launcher/src/browser/java.rs jar_title"),
)


def function(path: str, name: str) -> str:
    """The text of `fn name` in the Rust file at `path`, from `fn` to its closing brace."""
    source = (ROOT / path).read_text(encoding="utf-8")
    found = re.search(rf"\bfn {name}\b", source)
    if not found:
        raise AssertionError(f"{path} has no fn {name}")
    depth, at = 0, source.index("{", found.end())
    while at < len(source):
        rest = source[at:]
        raw = re.match(r'r(#*)"', rest)
        char = re.match(r"'(\\.|[^\\'])'", rest)
        if rest.startswith("//"):
            at = source.find("\n", at)
        elif raw and (at == 0 or not (source[at - 1].isalnum() or source[at - 1] == "_")):
            at = source.index('"' + raw.group(1), at + len(raw.group(0))) + len(raw.group(1))
        elif rest.startswith('"'):
            at = re.compile(r'"(\\.|[^"\\])*"', re.S).match(source, at).end() - 1
        elif char:
            at += len(char.group(0)) - 1
        elif rest[0] in "{}":
            depth += 1 if rest[0] == "{" else -1
            if depth == 0:
                return source[found.start():at + 1]
        at += 1
    raise AssertionError(f"{path}: fn {name} never closes")


def code(text: str) -> str:
    """Rust source as this test compares it: no module docs, `use` items, whitespace or trailing commas."""
    text = re.sub(r"^\s*//!.*$", "", text, flags=re.M)
    text = re.sub(r"^\s*(pub(\(crate\))? )?use [^;]*;", "", text, flags=re.M | re.S)
    return re.sub(r",([)\]}])", r"\1", re.sub(r"\s+", "", text))


# (the game's file, the launcher's copy, the functions copied whole): compared as code, whole-line comments aside.
SAME_FUNCTIONS = (
    ("crates/sunrise-mod-package/src/sdk.rs", "launcher/crates/sdk/src/lib.rs", (
        "route", "to_json", "build_of", "schema", "read", "asset", "route_assets", "verify", "unpack", "lock",
        "no_locks", "rename_retrying", "busy", "extract", "untar", "entry_target",
    )),
)

# (the game's file, the launcher's copy, its edits, the types, impls and constants copied whole, by their head).
SAME_ITEMS = (
    ("crates/sunrise-mod-package/src/sdk.rs", "launcher/crates/sdk/src/lib.rs", (
        ('#[cfg(feature = "prebuilt")]', ""),
    ), (
        "pub const INDEX", "pub const SCHEMA", "pub struct Index", "pub struct Build", "pub struct Toolchain",
        "pub struct Surface", "pub struct ChangeNote", "pub enum Verdict", "pub struct Mixins", "pub struct Asset",
        "pub enum Kind", "impl Kind", "pub enum Codec", "pub enum Side", "impl Asset", "pub struct Inputs",
        "pub struct Route", "pub enum Refusal", "impl fmt::Display for Refusal", "impl std::error::Error for Refusal",
        "struct Written", "struct WrittenAsset", "pub const DICTIONARY_LIMIT", "pub const EXTENSION_LIMIT",
        "pub const LOCK_SUFFIX", "pub const PARTIAL_SUFFIX", "const PAX_KEYS", "struct Partial", "impl Partial",
        "impl Drop for Partial",
    )),
)


def item(path: str, head: str) -> str:
    """The text of the item starting `head` in the Rust file at `path`, from its attributes to its end."""
    source = (ROOT / path).read_text(encoding="utf-8")
    found = re.search(rf"^[ \t]*{re.escape(head)}\b", source, re.M)
    if not found:
        raise AssertionError(f"{path} has no {head}")
    start = found.start()
    while (above := re.search(r"^[ \t]*(#\[.*\]|//.*)\n\Z", source[:start], re.M)) is not None:
        start = above.start()
    depth, at = 0, found.end()
    while at < len(source):
        rest = source[at:]
        if rest.startswith("//"):
            at = source.find("\n", at)
        elif rest.startswith('"'):
            at = re.compile(r'"(\\.|[^"\\])*"', re.S).match(source, at).end() - 1
        elif rest[0] in "{[(":
            depth += 1
        elif rest[0] in "}])":
            depth -= 1
            if depth == 0 and rest[0] == "}":
                return source[start:at + 1]
        elif rest[0] == ";" and depth == 0:
            return source[start:at + 1]
        at += 1
    raise AssertionError(f"{path}: {head} never closes")


def uncommented(text: str) -> str:
    """`text` without its whole-line comments: a copy may say less than the game's comments."""
    return re.sub(r"^\s*//.*$", "", text, flags=re.M)


def files(path: Path) -> dict:
    if path.is_file():
        return {"": path.read_bytes()}
    return {str(f.relative_to(path)): f.read_bytes() for f in sorted(path.rglob("*")) if f.is_file()}


def value(expression: str):
    """A Rust constant's value: a string literal, or integers joined by <<, *, + and -."""
    expression = expression.strip()
    if expression.startswith('"'):
        return ast.literal_eval(expression)
    try:
        tree = ast.parse(re.sub(r"(\d)_(?=\d)", r"\1", expression), mode="eval").body
    except SyntaxError as error:
        raise ValueError(f"not a constant this test reads: {expression}") from error

    def evaluate(node):
        if isinstance(node, ast.Constant) and isinstance(node.value, int):
            return node.value
        if isinstance(node, ast.BinOp):
            ops = {ast.LShift: lambda a, b: a << b, ast.Mult: lambda a, b: a * b,
                   ast.Add: lambda a, b: a + b, ast.Sub: lambda a, b: a - b}
            return ops[type(node.op)](evaluate(node.left), evaluate(node.right))
        raise ValueError(f"not a constant this test reads: {expression}")

    return evaluate(tree)


def constant(path: str, name: str):
    source = (ROOT / path).read_text(encoding="utf-8")
    found = re.search(rf"\bconst {name}\s*:\s*[^=]+=\s*(.+?);", source, re.S)
    if not found:
        raise AssertionError(f"{path} has no const {name}")
    return value(found.group(1))


class LauncherCopies(unittest.TestCase):
    def test_copied_files_are_the_games_byte_for_byte(self):
        for game, copy in SAME_BYTES:
            with self.subTest(copy=copy):
                original, copied = files(ROOT / game), files(COPIES / copy)
                self.assertTrue(original, f"{game} is empty or missing")
                self.assertEqual(sorted(copied), sorted(original), f"{copy} lists other files than {game}")
                for name in original:
                    where = f"{name} in " if name else ""
                    self.assertEqual(copied[name], original[name], f"{where}{copy} differs from {game}")

    def test_rewritten_copies_are_the_games_code_with_their_listed_edits(self):
        for game, copy, edits in SAME_CODE:
            with self.subTest(copy=copy):
                original = code((ROOT / game).read_text(encoding="utf-8"))
                for old, new in edits:
                    self.assertIn(code(old), original, f"{game} no longer has the code {copy} edits: update both")
                    original = original.replace(code(old), code(new))
                self.assertEqual(code((ROOT / copy).read_text(encoding="utf-8")), original,
                                 f"{copy} differs from {game} beyond its listed edits: copy the change, or list it")

    def test_mirrored_game_functions_are_unchanged(self):
        for path, name, pinned, mirror in MIRRORED:
            with self.subTest(function=name):
                digest = hashlib.sha256(code(function(path, name)).encode()).hexdigest()
                self.assertEqual(digest, pinned, f"{path}'s {name} changed: change {mirror} to match, then pin {digest}")

    def test_functions_copied_whole_are_the_games(self):
        for game, copy, names in SAME_FUNCTIONS:
            for name in names:
                with self.subTest(function=name):
                    self.assertEqual(code(uncommented(function(copy, name))), code(uncommented(function(game, name))),
                                     f"{copy}'s {name} differs from {game}'s: copy the change")

    def test_items_copied_whole_are_the_games(self):
        for game, copy, edits, heads in SAME_ITEMS:
            for head in heads:
                with self.subTest(item=head):
                    original = code(uncommented(item(game, head)))
                    for old, new in edits:
                        original = original.replace(code(old), code(new))
                    self.assertEqual(code(uncommented(item(copy, head))), original,
                                     f"{copy}'s {head} differs from {game}'s: copy the change")

    def test_format_constants_match_the_games(self):
        package_format = int((ROOT / "crates/sunrise-mod-package/src/sdk/package-format").read_text().strip())
        self.assertEqual(constant("launcher/crates/smod/src/index/native.rs", "PACKAGE_FORMAT"), package_format)
        self.assertEqual(constant("launcher/crates/sdk/src/lib.rs", "PACKAGE_FORMAT"), package_format)
        self.assertEqual(constant("launcher/crates/smod/src/manifest.rs", "USES_FILE"),
                         constant("crates/sunrise-mod-surface/src/uses.rs", "USES_FILE"))
        self.assertEqual(constant("launcher/crates/smod/src/manifest.rs", "MAX_USES"),
                         constant("crates/sunrise-mod-surface/src/uses.rs", "MAX_USES_BYTES"))

    def test_a_drifted_copy_is_caught(self):
        self.assertEqual(code("//! Doc.\nuse a::{b,\n c};\nfn f() { g( 1 ) }\n"), "fnf(){g(1)}")
        self.assertNotEqual(code("fn f() { g(1) }"), code("fn f() { g(2) }"))
        self.assertEqual(value("4 << 20"), 4 * 1024 * 1024)
        self.assertEqual(value("1_000"), 1000)
        self.assertEqual(value('"uses.json"'), "uses.json")
        with self.assertRaises(ValueError):
            value("mod_surface::uses::MAX_USES_BYTES as u64")
        self.assertEqual(code(uncommented("fn f() {\n    // Why.\n    g(1)\n}")), "fn f(){g(1)}".replace(" ", ""))


if __name__ == "__main__":
    unittest.main()
