use bincode::{DefaultOptions, Options};
use core::time;
use daemonize::Daemonize;
use dirs::config_dir;
use nix::libc;
use nix::libc::{c_ushort, TIOCSWINSZ};
use nix::sys::wait::waitpid;
use nix::unistd::ForkResult;
use pty::fork::*;
use serde::{Deserialize, Serialize};
use signal_hook::{
    consts::{SIGINT, SIGWINCH},
    iterator::Signals,
};
use std::collections::HashMap;
use std::fs::{self, remove_file};
use std::io::{self, stdout, Read};
use std::io::{ErrorKind, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::prelude::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::{env, fmt, thread};
use termion::raw::IntoRawMode;
use timeout_readwrite::TimeoutReader;

use anyhow::Context;

fn server(
    socket_path: String,
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
) -> anyhow::Result<()> {
    env.get("PWD").map(env::set_current_dir);
    for (k, v) in env {
        env::set_var(k, v);
    }
    if std::fs::metadata(&socket_path).is_ok() {
        println!("A socket is already present. Deleting...");
        std::fs::remove_file(&socket_path)
            .with_context(|| format!("could not delete previous socket at {:?}", socket_path))?;
    }

    let unix_listener =
        UnixListener::bind(&socket_path).context("Could not create the unix socket")?;

    let socket_path2 = socket_path.clone();

    let mut signals = Signals::new(&[SIGINT])?;

    thread::spawn(move || {
        for _ in signals.forever() {
            println!("unlink2 {}", &socket_path2);
            remove_file(&socket_path2).unwrap();
        }
    });

    let daemonize = Daemonize::new(); // Redirect stderr to `/tmp/daemon.err`.

    let dir = env::current_dir()?;
    daemonize.working_directory(dir).start()?;

    let fork = Fork::from_ptmx().unwrap();
    if let Ok(master) = fork.is_parent() {
        thread::spawn(move || loop {
            waitpid(None, None).unwrap();
            println!("unlink {}", &socket_path);
            remove_file(&socket_path).unwrap();
            std::process::exit(0);
        });
        // put the server logic in a loop to accept several connections
        loop {
            let (unix_stream, _socket_address) = unix_listener
                .accept()
                .context("Failed at accepting a connection on the unix listener")?;
            handle_stream(unix_stream, master)?;
        }
    } else {
        Command::new(command).args(args).exec();
    }
    Ok(())
}

#[derive(Debug)]
#[repr(C)]
struct UnixSize {
    ws_row: c_ushort,
    ws_col: c_ushort,
    ws_xpixel: c_ushort,
    ws_ypixel: c_ushort,
}

#[derive(Serialize, Deserialize, Debug)]
struct Message {
    mode: u8,
    size: (u16, u16),
    bytes: Vec<u8>,
}

fn send_message(unix_stream: &mut UnixStream, message: &Message) -> anyhow::Result<()> {
    let encoded = DefaultOptions::new()
        .with_varint_encoding()
        .serialize(&message)?;
    unix_stream.write_all(&[encoded.len() as u8])?;
    unix_stream.write_all(&encoded[..]).map_err(|x| x.into())
}

fn receive_message(unix_stream_reader: &mut UnixStream) -> anyhow::Result<Message> {
    let mut len_array = vec![0; 1];
    unix_stream_reader.read_exact(&mut len_array)?;
    let mut bytes = vec![0; len_array[0].into()];
    unix_stream_reader.read_exact(&mut bytes)?;
    DefaultOptions::new()
        .with_varint_encoding()
        .deserialize_from(&bytes[..])
        .map_err(|x| x.into())
}

fn handle_stream(mut unix_stream: UnixStream, mut master: Master) -> anyhow::Result<()> {
    let mut master_reader = master;
    let mut unix_stream_reader = unix_stream.try_clone()?;
    let fd = master.as_raw_fd();
    thread::spawn(move || {
        loop {
            let message = receive_message(&mut unix_stream_reader).unwrap();
            if message.mode == 0 {
                let us = UnixSize {
                    ws_row: message.size.1,
                    ws_col: message.size.0,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
                unsafe {
                    libc::ioctl(fd, TIOCSWINSZ, &us);
                };
            } else if message.mode == 1 {
                if master.write(&message.bytes).is_err() {
                    break;
                }
            } else if message.mode == 2 {
                // detach
            }
        }
    });
    let mut bytesr = [0; 1024];
    thread::spawn(move || loop {
        let size = master_reader
            .read(&mut bytesr)
            .context("failed at reading stdout")
            .unwrap();
        if size > 0 {
            let res = unix_stream.write(&bytesr[0..size]);
            if res.is_err() {
                break;
            }
            res.context("Failed at writing the unix stream").unwrap();
        } else {
            break;
        }
    });

    Ok(())
}

fn escape_key_to_byte(escape_key: Option<String>) -> u8 {
    let allowed_keys = vec![
        "a".to_string(),
        "b".to_string(),
        "c".to_string(),
        "d".to_string(),
        "e".to_string(),
        "f".to_string(),
        "g".to_string(),
    ];
    escape_key
        .map(|x| {
            allowed_keys
                .iter()
                .position(|y| y == &x)
                .map(|i| i as u8 + 1)
                .unwrap_or(1)
        })
        .unwrap_or(1)
}

fn client(socket_path: String, escape_key: Option<String>) -> anyhow::Result<()> {
    let mut unix_stream = UnixStream::connect(socket_path).context("Could not create stream")?;

    write_request_and_shutdown(&mut unix_stream, escape_key_to_byte(escape_key))?;
    // read_from_stream(&mut unix_stream)?;
    Ok(())
}

fn write_request_and_shutdown(unix_stream: &mut UnixStream, escape_code: u8) -> anyhow::Result<()> {
    let mut _stdout = stdout().into_raw_mode()?;
    let mut stdin = io::stdin();

    let mut unix_stream_reader =
        TimeoutReader::new(unix_stream.try_clone()?, Duration::from_millis(200));

    print!("{}[2J", 27 as char);
    let mut _stdout2 = stdout();
    let done = Arc::new(AtomicBool::new(false));

    let done_in = done.clone();
    thread::spawn(move || {
        let mut bytes = [0; 255];
        loop {
            if done_in.load(Ordering::Relaxed) {
                break;
            }
            match unix_stream_reader.read(&mut bytes) {
                Ok(_size) => {
                    if _size > 0 {
                        _stdout2
                            .write(&bytes[0.._size])
                            .context("failed at writing stdin")
                            .unwrap();
                        _stdout2.flush().unwrap();
                    }
                }
                Err(ref e) if e.kind() == ErrorKind::TimedOut => {}
                Err(_) => break,
            }
        }
    });

    let mut signals = Signals::new(&[SIGWINCH])?;
    let mut unix_stream_resize = unix_stream.try_clone()?;

    thread::spawn(move || {
        for sig in signals.forever() {
            if sig == SIGWINCH
                && send_message(
                    &mut unix_stream_resize,
                    &Message {
                        mode: 0,
                        size: termion::terminal_size().unwrap(),
                        bytes: vec![0],
                    },
                )
                .is_err()
            {
                break;
            }
        }
    });

    // send terminal size
    send_message(
        unix_stream,
        &Message {
            mode: 0,
            size: termion::terminal_size()?,
            bytes: vec![0],
        },
    )
    .context("Failed at writing the unix stream")?;

    unix_stream.flush()?;
    // send CTRL+L to force redraw
    send_message(
        unix_stream,
        &Message {
            mode: 1,
            size: (0, 0),
            bytes: vec![12],
        },
    )
    .context("Failed at writing the unix stream")?;
    let mut unix_stream_stdin = unix_stream.try_clone()?;

    let mut bytesr = [0; 1024];
    let t2 = thread::spawn(move || 'outer: loop {
        let _size = stdin
            .read(&mut bytesr)
            .context("failed at reading stdout")
            .unwrap();
        if _size > 0 {
            if _size == 1 && bytesr[0] == escape_code {
                // detach
                send_message(
                    &mut unix_stream_stdin,
                    &Message {
                        mode: 2,
                        size: (0, 0),
                        bytes: vec![bytesr[0]],
                    },
                )
                .context("Failed at writing the unix stream")
                .unwrap();
                unix_stream_stdin
                    .shutdown(std::net::Shutdown::Write)
                    .context("Could not shutdown writing on the stream")
                    .unwrap();
            }
            let res = send_message(
                &mut unix_stream_stdin,
                &Message {
                    mode: 1,
                    size: (0, 0),
                    bytes: bytesr[.._size].into(),
                },
            );
            if res.is_err() {
                break 'outer;
            }
        }
    });

    t2.join().unwrap();

    _stdout.suspend_raw_mode()?;

    done.store(true, Ordering::Relaxed);

    unix_stream
        .shutdown(std::net::Shutdown::Write)
        .context("Could not shutdown writing on the stream")?;

    unix_stream
        .shutdown(std::net::Shutdown::Read)
        .context("Could not shutdown writing on the stream")?;

    Ok(())
}

struct NoConfigDir;

impl fmt::Display for NoConfigDir {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "no config dir")
    }
}

fn conf_dir() -> anyhow::Result<String> {
    let dir = config_dir().ok_or_else(|| anyhow::anyhow!("config dir not found"))?;
    let crate_name = option_env!("CARGO_PKG_NAME").unwrap_or("rda");
    let dir_str = dir.as_path().display();
    let confdir = format!("{}/{}", dir_str, crate_name);
    if !Path::new(&confdir).exists() {
        fs::create_dir(&confdir)?;
    }
    Ok(confdir)
}

fn session_name_to_socket_path(session_name: String) -> anyhow::Result<String> {
    let confdir = conf_dir()?;
    Ok(format!("{}/{}", confdir, session_name))
}

pub fn list_sessions() -> anyhow::Result<Vec<String>> {
    let paths = fs::read_dir(conf_dir()?)?;
    let res = paths
        .map(|path| {
            path.unwrap()
                .file_name()
                .to_str()
                .unwrap_or("failed to unwrap file name")
                .to_string()
        })
        .collect();
    Ok(res)
}

fn session_running(session_name: String) -> anyhow::Result<bool> {
    Ok(Path::new(&session_name_to_socket_path(session_name)?).exists())
}

pub fn server_client(
    session_name: &str,
    command: &[String],
    env: HashMap<String, String>,
    escape_key: Option<String>,
) -> anyhow::Result<()> {
    let socket_path = session_name_to_socket_path(session_name.to_string())?;
    if session_running(session_name.to_string())? {
        client(socket_path, escape_key)?;
    } else {
        let pid = unsafe { nix::unistd::fork() };
        match pid.expect("Fork Failed: Unable to create child process!") {
            ForkResult::Child => {
                let command_name = command.get(0).unwrap();
                let remaining_args = &command[1..command.len()];
                server(
                    socket_path,
                    command_name.to_string(),
                    remaining_args.to_vec(),
                    env,
                )?;
            }
            ForkResult::Parent { .. } => {
                thread::sleep(time::Duration::from_millis(100));
                client(socket_path, escape_key)?;
            }
        }
    }
    Ok(())
}
