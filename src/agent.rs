use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

pub type Obj = BTreeMap<String, Json>;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Obj),
}

impl Json {
    pub fn str(&self) -> Option<&str> {
        if let Json::Str(s) = self { Some(s) } else { None }
    }
}

impl From<&str> for Json {
    fn from(s: &str) -> Json {
        Json::Str(s.into())
    }
}

impl From<String> for Json {
    fn from(s: String) -> Json {
        Json::Str(s)
    }
}

impl From<u64> for Json {
    fn from(n: u64) -> Json {
        Json::Num(n as f64)
    }
}

impl From<bool> for Json {
    fn from(b: bool) -> Json {
        Json::Bool(b)
    }
}

impl std::fmt::Display for Json {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Json::Null => f.write_str("null"),
            Json::Bool(b) => write!(f, "{b}"),
            Json::Num(n) => write!(f, "{n}"),
            Json::Str(s) => {
                f.write_str("\"")?;
                for c in s.chars() {
                    match c {
                        '"' => f.write_str("\\\"")?,
                        '\\' => f.write_str("\\\\")?,
                        '\n' => f.write_str("\\n")?,
                        '\r' => f.write_str("\\r")?,
                        '\t' => f.write_str("\\t")?,
                        c if (c as u32) < 0x20 => write!(f, "\\u{:04x}", c as u32)?,
                        c => write!(f, "{c}")?,
                    }
                }
                f.write_str("\"")
            }
            Json::Arr(a) => {
                f.write_str("[")?;
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str("]")
            }
            Json::Obj(o) => {
                f.write_str("{")?;
                for (i, (k, v)) in o.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{}:{v}", Json::Str(k.clone()))?;
                }
                f.write_str("}")
            }
        }
    }
}

pub fn parse(s: &str) -> Result<Json, String> {
    let mut p = Parser { s: s.as_bytes(), i: 0 };
    let v = p.value()?;
    p.ws();
    if p.i != p.s.len() {
        return Err(format!("trailing data at byte {}", p.i));
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.s.get(self.i).is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Result<(), String> {
        self.ws();
        if self.s.get(self.i) == Some(&c) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {}", c as char, self.i))
        }
    }

    fn lit(&mut self, word: &str, v: Json) -> Result<Json, String> {
        if self.s[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(format!("bad literal at byte {}", self.i))
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        self.ws();
        match self.s.get(self.i) {
            Some(b'{') => {
                self.i += 1;
                let mut o = Obj::new();
                self.ws();
                if self.s.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Json::Obj(o));
                }
                loop {
                    self.ws();
                    let Json::Str(k) = self.value()? else { return Err("object keys must be strings".into()) };
                    self.eat(b':')?;
                    o.insert(k, self.value()?);
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Json::Obj(o));
                        }
                        _ => return Err(format!("expected ',' or '}}' at byte {}", self.i)),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut a = Vec::new();
                self.ws();
                if self.s.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Json::Arr(a));
                }
                loop {
                    a.push(self.value()?);
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Json::Arr(a));
                        }
                        _ => return Err(format!("expected ',' or ']' at byte {}", self.i)),
                    }
                }
            }
            Some(b'"') => self.string().map(Json::Str),
            Some(b't') => self.lit("true", Json::Bool(true)),
            Some(b'f') => self.lit("false", Json::Bool(false)),
            Some(b'n') => self.lit("null", Json::Null),
            Some(_) => {
                let start = self.i;
                while self.s.get(self.i).is_some_and(|c| matches!(c, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')) {
                    self.i += 1;
                }
                std::str::from_utf8(&self.s[start..self.i])
                    .ok()
                    .and_then(|n| n.parse().ok())
                    .map(Json::Num)
                    .ok_or_else(|| format!("bad value at byte {start}"))
            }
            None => Err("unexpected end of input".into()),
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1;
        let mut out = Vec::new();
        loop {
            let Some(&c) = self.s.get(self.i) else { return Err("unterminated string".into()) };
            self.i += 1;
            match c {
                b'"' => return String::from_utf8(out).map_err(|_| "string is not utf-8".into()),
                b'\\' => {
                    let Some(&e) = self.s.get(self.i) else { return Err("unterminated string".into()) };
                    self.i += 1;
                    let ch = match e {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'u' => {
                            let mut code = self.hex4()?;
                            if (0xd800..0xdc00).contains(&code) && self.s[self.i..].starts_with(b"\\u") {
                                self.i += 2;
                                let low = self.hex4()?;
                                code = 0x10000 + ((code - 0xd800) << 10) + (low.wrapping_sub(0xdc00) & 0x3ff);
                            }
                            char::from_u32(code).unwrap_or('\u{fffd}')
                        }
                        c => c as char,
                    };
                    let mut b = [0; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
                }
                c => out.push(c),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self.s.get(self.i..self.i + 4).and_then(|h| std::str::from_utf8(h).ok()).ok_or("bad \\u escape")?;
        self.i += 4;
        u32::from_str_radix(h, 16).map_err(|_| "bad \\u escape".into())
    }
}

pub fn obj<const N: usize>(pairs: [(&str, Json); N]) -> Json {
    Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

pub fn err(msg: impl Into<String>) -> Json {
    obj([("ok", false.into()), ("error", Json::Str(msg.into()))])
}

pub fn dir() -> PathBuf {
    let tmp = std::env::var_os("TMPDIR").map(PathBuf::from).unwrap_or_else(|| "/tmp".into());
    tmp.join(format!("{}-{}", env!("CARGO_BIN_NAME"), unsafe { libc::getuid() }))
}

pub struct Request {
    pub body: Obj,
    pub reply: Sender<Json>,
}

pub struct Server {
    path: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn listen(tx: Sender<Request>) -> std::io::Result<Server> {
    let dir = dir();
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let path = dir.join(format!("{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            std::thread::spawn(move || serve(stream, tx));
        }
    });
    Ok(Server { path })
}

/// Hands `body` to the TUI and waits for its answer.
pub fn ask(tx: &Sender<Request>, body: Obj) -> Json {
    let (reply, rx) = channel();
    if tx.send(Request { body, reply }).is_err() {
        return err("the TUI has quit");
    }
    rx.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| err("the TUI did not answer in time"))
}

fn serve(stream: UnixStream, tx: Sender<Request>) {
    let Ok(mut out) = stream.try_clone() else { return };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let answer = match parse(&line) {
            Ok(Json::Obj(body)) => ask(&tx, body),
            Ok(_) => err("request must be a JSON object"),
            Err(e) => err(format!("bad JSON: {e}")),
        };
        if writeln!(out, "{answer}").is_err() {
            return;
        }
    }
}

pub fn call(socket: &Path, req: &Json) -> std::io::Result<Json> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    writeln!(stream, "{req}")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    parse(line.trim()).map_err(std::io::Error::other)
}

pub struct Instance {
    pub pid: u32,
    started: std::time::SystemTime,
    pub socket: PathBuf,
    pub root: PathBuf,
}

pub fn instances() -> Vec<Instance> {
    let Ok(entries) = std::fs::read_dir(dir()) else { return Vec::new() };
    let mut found = Vec::new();
    for e in entries.flatten() {
        let socket = e.path();
        let Some(pid) = socket.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse().ok()) else { continue };
        if socket.extension().is_none_or(|x| x != "sock") {
            continue;
        }
        let started = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
        match call(&socket, &obj([("cmd", "hello".into())])) {
            Ok(Json::Obj(o)) => {
                let root = o.get("root").and_then(Json::str).unwrap_or_default().into();
                found.push(Instance { pid, started, socket, root });
            }
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused || e.kind() == std::io::ErrorKind::NotFound => {
                let _ = std::fs::remove_file(&socket);
            }
            _ => {}
        }
    }
    found.sort_by_key(|i| std::cmp::Reverse(i.started));
    found
}

pub fn pick<'a>(all: &'a [Instance], path: Option<&Path>) -> Option<&'a Instance> {
    let Some(path) = path else { return all.first() };
    all.iter().rev().filter(|i| path.starts_with(&i.root)).max_by_key(|i| i.root.components().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_objects() {
        let v = obj([
            ("cmd", "suggest".into()),
            ("path", "/tmp/a \"b\"\\c\n".into()),
            ("n", 42u64.into()),
            ("list", Json::Arr(vec![Json::Null, true.into()])),
        ]);
        assert_eq!(parse(&v.to_string()).unwrap(), v);
    }

    #[test]
    fn parses_escapes_and_whitespace() {
        let v = parse(r#" { "a" : "\u00e9\ud83d\ude00\t" , "b" : [ 1.5 , -2 ] } "#).unwrap();
        let Json::Obj(o) = v else { panic!() };
        assert_eq!(o["a"], Json::Str("é😀\t".into()));
        assert_eq!(o["b"], Json::Arr(vec![Json::Num(1.5), Json::Num(-2.0)]));
    }

    #[test]
    fn rejects_bad_json() {
        assert!(parse("{\"a\":1").is_err());
        assert!(parse("{\"a\":1} x").is_err());
        assert!(parse("{1:2}").is_err());
        assert!(parse("\"open").is_err());
    }

    #[test]
    fn picks_deepest_root_containing_path() {
        let inst = |pid, root: &str| Instance { pid, started: std::time::UNIX_EPOCH, socket: PathBuf::new(), root: root.into() };
        let all = vec![inst(40, "/Users/t"), inst(30, "/Users/t/Code"), inst(20, "/Users/t"), inst(10, "/Users/t/Code/space")];
        assert_eq!(pick(&all, Some(Path::new("/Users/t/Code/space/src"))).unwrap().pid, 10);
        assert_eq!(pick(&all, Some(Path::new("/Users/t/Downloads"))).unwrap().pid, 40);
        assert!(pick(&all, Some(Path::new("/opt"))).is_none());
        assert_eq!(pick(&all, None).unwrap().pid, 40);
    }
}
