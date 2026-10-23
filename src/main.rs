use std::{
    fs::OpenOptions,
    io::{BufRead, BufReader, Stdin, Stdout, Write},
    os::fd::AsRawFd,
    path::PathBuf,
    process::Command,
};

#[derive(Debug)]
struct Context {
    stdout: Stdout,
    stdin: Stdin,
    prompt: Option<String>,
    default_prompt: Option<String>,
    description: Option<String>,
    error: Option<String>,
    ttyname: PathBuf,
    pid: u32,
}

impl Context {
    fn new() -> Self {
        Self {
            stdin: std::io::stdin(),
            stdout: std::io::stdout(),
            prompt: None,
            default_prompt: None,
            description: None,
            error: None,
            ttyname: PathBuf::from("/dev/tty"),
            pid: std::process::id(),
        }
    }

    fn reply(&self, message: impl AsRef<str>) {
        writeln!(self.stdout.lock(), "{}", message.as_ref()).expect("send")
    }

    fn read<'a>(&self, buffer: &'a mut String) -> Option<(&'a str, &'a str)> {
        buffer.clear();
        match self.stdin.lock().read_line(buffer) {
            Ok(0) => None,
            Ok(_) => {
                let line = buffer.trim_end_matches(['\r', '\n']);
                Some(line.split_once(' ').unwrap_or((line, "")))
            }
            Err(err) => {
                self.reply(format!("ERR 1 {}", percent_encode(format!("{err}"))));
                None
            }
        }
    }

    fn option(&mut self, option: &str) {
        let (name, value) = option.split_once('=').unwrap_or((option, ""));
        match name {
            "ttyname" if !value.is_empty() => {
                self.ttyname = percent_decode(value)
                    .map(|s| PathBuf::from(s))
                    .unwrap_or_else(|| PathBuf::from("/dev/tty"));
            }
            "default-prompt" => self.default_prompt = percent_decode(value),
            "owner" => {
                self.pid = value
                    .split_once('/')
                    .and_then(|(pid, _)| pid.parse().ok())
                    .unwrap_or(std::process::id());
            }
            _ => {}
        }
        self.reply("OK");
    }

    fn set_description(&mut self, description: &str) {
        self.description = percent_decode(description);
        self.reply("OK");
    }

    fn set_error(&mut self, error: &str) {
        self.error = percent_decode(error);
        self.reply("OK");
    }

    fn set_prompt(&mut self, prompt: &str) {
        self.prompt = percent_decode(prompt);
        self.reply("OK");
    }

    fn get_info(&self, info: &str) {
        match info {
            "flavor" => {
                self.reply("D tty");
                self.reply("OK")
            }
            "version" => {
                self.reply(format!("D {}", env!("CARGO_PKG_VERSION")));
                self.reply("OK")
            }
            "pid" => {
                self.reply(format!("D {}", std::process::id()));
                self.reply("OK")
            }
            "ttyinfo" => {
                self.reply("D 0 0 0 0");
                self.reply("OK")
            }
            _ => self.reply("ERR 1 unknown info"),
        }
    }

    fn get_pin(&self) {
        if self.env_has_display() {
            self.read_password_terminal()
        } else {
            self.read_password_tty()
        }
    }

    fn read_password_terminal(&self) {
        let output = Command::new("sh")
            .arg("-c")
            .arg(
                r#"exec xdg-terminal-exec --app-id=pinentry-terminal sh -c '
                printf "%s" "$1"
                if IFS= read -r -s pass; then
                    printf "%s" "$pass" > /proc/'"$$"'/fd/1
                    exit 0
                else
                    exit 2
                fi
            ' sh "$1""#,
            )
            .arg("sh")
            .arg(self.render())
            .output();
        match output {
            Ok(output) => match output.status.code() {
                Some(0) => {
                    self.reply(format!("D {}", percent_encode_bytes(&output.stdout)));
                    self.reply("OK");
                }
                Some(2) => self.reply("ERR 99 canceled"),
                Some(c) => self.reply(format!("ERR 1 reader exited with {c}")),
                None => self.reply("ERR 1 reader killed by signal"),
            },
            Err(err) => self.reply(format!("ERR 1 {}", percent_encode(format!("{err}")))),
        }
    }

    fn read_password_tty(&self) {
        let Ok(mut tty) = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.ttyname)
        else {
            return self.reply(format!(
                "ERR 1 could not open {}",
                percent_encode(&self.ttyname.to_string_lossy())
            ));
        };

        let fd = tty.as_raw_fd();

        let mut saved = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return self.reply("ERR 1 could not save stty");
        }

        let mut quiet = saved;
        quiet.c_lflag &= !libc::ECHO;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) } != 0 {
            return self.reply("ERR 1 could not disable stty echo");
        }

        if write!(tty, "{}", self.render()).is_err() {
            if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) } != 0 {
                return self.reply("ERR 1 could not restore stty");
            } else {
                return self.reply(format!(
                    "ERR 1 could not write to {}",
                    percent_encode(&self.ttyname.to_string_lossy())
                ));
            }
        };

        let mut buffer = Vec::new();
        let Ok(res) = BufReader::new(&tty).read_until(b'\n', &mut buffer) else {
            if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) } != 0 {
                return self.reply("ERR 1 could not restore stty");
            } else {
                return self.reply(format!(
                    "ERR 1 could not read line from {}",
                    percent_encode(&self.ttyname.to_string_lossy())
                ));
            }
        };

        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) } != 0 {
            return self.reply("ERR 1 could not restore stty");
        }

        if writeln!(tty).is_err() {
            return self.reply(format!(
                "ERR 1 could not write to {}",
                percent_encode(&self.ttyname.to_string_lossy())
            ));
        };

        match res {
            1.. => {
                while matches!(buffer.last(), Some(b'\r' | b'\n')) {
                    buffer.pop();
                }
                self.reply(format!("D {}", percent_encode_bytes(&buffer)));
                self.reply("OK");
            }
            0 => self.reply("ERR 99 canceled"),
        }
    }

    fn render(&self) -> String {
        let mut out = String::new();
        if let Some(error) = self.error.as_ref() {
            out.push_str(&error);
            out.push_str("\n");
        }
        if let Some(description) = self.description.as_ref() {
            out.push_str(&description);
            out.push_str("\n");
        }
        if let Some(prompt) = self.prompt.as_ref() {
            out.push_str(&prompt);
        } else if let Some(default_prompt) = self.default_prompt.as_ref() {
            out.push_str(&default_prompt);
        }
        out
    }

    fn env_has_display(&self) -> bool {
        std::fs::read(format!("/proc/{}/environ", self.pid))
            .unwrap_or_default()
            .split(|&b| b == 0)
            .any(|v| v.starts_with(b"DISPLAY=") || v.starts_with(b"WAYLAND_DISPLAY="))
    }
}

fn main() {
    let mut ctx = Context::new();
    let mut buffer = String::new();
    ctx.reply("OK Pleased to meet you");
    loop {
        match ctx.read(&mut buffer) {
            Some((cmd, arg)) => match cmd {
                "OPTION" => ctx.option(arg),
                "SETDESC" => ctx.set_description(arg),
                "SETERROR" => ctx.set_error(arg),
                "SETPROMPT" => ctx.set_prompt(arg),
                "GETINFO" => ctx.get_info(arg),
                "GETPIN" => ctx.get_pin(),
                "SETKEYINFO" => ctx.reply("OK"),
                "BYE" => {
                    ctx.reply("OK");
                    break;
                }
                _ => ctx.reply("ERR 1 unknown command"),
            },
            None => break,
        };
    }
}

fn percent_encode(s: impl AsRef<str>) -> String {
    percent_encode_bytes(s.as_ref().as_bytes())
}

fn percent_decode(s: impl AsRef<str>) -> Option<String> {
    percent_decode_bytes(s.as_ref().as_bytes())
}

fn percent_encode_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for &b in bytes {
        match b {
            b'%' => out.push_str("%25"),
            b if b < 0x20 || b > 0x7e => out.push_str(&format!("%{b:02X}")),
            b => out.push(b as char),
        }
    }
    out
}

fn percent_decode_bytes(bytes: &[u8]) -> Option<String> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if let [b'%', h, l, ..] = &bytes[i..]
            && let (Some(h), Some(l)) = ((*h as char).to_digit(16), (*l as char).to_digit(16))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}
