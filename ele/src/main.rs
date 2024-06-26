use std::{collections::HashMap, env, io::IsTerminal, os::fd::{AsFd, AsRawFd}};

use argh::{from_env, FromArgs};
use log::debug;
use nix::{errno::Errno, sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg, Termios}, unistd::isatty};
use pty_process::Pty;
use tokio::{
    io::{copy, copy_bidirectional, join, stderr, stdin, stdout, Join, Stdin, Stdout},
    process::{ChildStderr, ChildStdin, ChildStdout},
    signal::unix::{signal, SignalKind},
};
use zbus::{proxy, zvariant::OwnedFd, Connection, Result};

#[derive(Debug, FromArgs)]
/// Top-level command.
struct Cli {
    /// what user to run the program as
    #[argh(option, default = "\"root\".to_string()")]
    user: String,

    /// whether to attach the process to a pty
    #[argh(switch, short = 'i')]
    interactive: bool,

    /// the appliation to run
    #[argh(positional)]
    program: String,

    /// the arguments to pass to it
    #[argh(positional, greedy)]
    args: Vec<String>,
}

#[proxy(
    interface = "de.ytvwld.Ele1",
    default_service = "de.ytvwld.Ele",
    default_path = "/de/ytvwld/Ele"
)]
trait EleD {
    async fn create(&self, user: &str, argv: Vec<String>, interactive: bool) -> Result<String>;
}

#[proxy(
    interface = "de.ytvwld.Ele1.Process",
    default_service = "de.ytvwld.Ele",
)]
trait EleProcess {
    async fn environment(&self, environ: HashMap<String, String>) -> Result<()>;
    async fn directory(&self, path: &str) -> Result<()>;
    async fn signal(&self, signal: i32) -> Result<()>;
    async fn spawn(&self) -> Result<Vec<OwnedFd>>;
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", "info");
    }
    env_logger::init();
    let cli: Cli = from_env();
    debug!("Establishing connection to dbus...");
    let connection = Connection::system().await?;
    let eled_proxy = EleDProxy::new(&connection).await?;
    debug!("Waiting for authorization...");
    let mut args = cli.args.clone();
    args.insert(0, cli.program);
    let path = eled_proxy.create(&cli.user, args, cli.interactive).await?;
    let process = EleProcessProxy::builder(&connection)
        .path(path)?
        .build().await?;
    debug!("passing current directory...");
    process.directory(
        env::current_dir()?.to_str()
            .expect("failed to get current directory")
    ).await?;
    debug!("passing environment...");
    process.environment(
        HashMap::from_iter(env::vars())
    ).await?;
    // TODO: environment and resize
    debug!("Spawning process...");
    let attached_to = process.spawn().await?;
    let mut stdin = stdin();
    let mut stdout = stdout();
    if cli.interactive {
        let fd = attached_to.into_iter().next().unwrap();
        assert!(fd.as_fd().is_terminal());
        let mut pty = unsafe { Pty::from_raw_fd(fd.as_raw_fd()).unwrap() };
        let mut terminal = join(&mut stdin, &mut stdout);
        // set the tty as raw
        let old_attrs = set_raw(&mut terminal)?;
        // in a raw tty, the shell on the other side will handle ^c and ^z,
        // so we don't have to
        copy_bidirectional(&mut pty, &mut terminal).await?;
        // restore the terminal configuration
        if let Some(attrs) = old_attrs {
            reset_terminal(&mut terminal, attrs)?;
        }
    } else {
        let mut fd_iter = attached_to.into_iter();
        let mut child_stdin = ChildStdin::from_std(std::process::ChildStdin::from(
            std::os::fd::OwnedFd::from(fd_iter.next().unwrap())
        ))?;
        let mut child_stdout = ChildStdout::from_std(std::process::ChildStdout::from(
            std::os::fd::OwnedFd::from(fd_iter.next().unwrap())
        ))?;
        let mut child_stderr = ChildStderr::from_std(std::process::ChildStderr::from(
            std::os::fd::OwnedFd::from(fd_iter.next().unwrap())
        ))?;
        let mut stderr = stderr();
        // we have to pass signals over
        tokio::spawn(async move {
            let kind = SignalKind::interrupt();
            let mut stream = signal(kind).unwrap();
            loop {
                stream.recv().await;
                process.signal(kind.as_raw_value() as i32).await.unwrap();
            }
        });
        tokio::spawn(async move { copy(&mut stdin, &mut child_stdin).await });
        tokio::spawn(async move { copy(&mut child_stdout, &mut stdout).await });
        copy(&mut child_stderr, &mut stderr).await?;
    }

    Ok(())
}


/// Sets the tty to raw mode (if it is a tty).
/// 
/// Returns the original mode.
fn set_raw(
    terminal: &mut Join<&mut Stdin, &mut Stdout>,
) -> std::result::Result<Option<Termios>, Errno> {
    if !isatty(terminal.reader().as_raw_fd())? {
        debug!("stdin is not connected to a tty, not modifying it");
        return Ok(None);
    }
    if !isatty(terminal.writer().as_raw_fd())? {
        debug!("stdout is not connected to a tty, not modifying it");
        return Ok(None);
    }
    let old_attrs = tcgetattr(terminal.writer())?;
    let mut new_attrs = old_attrs.clone();
    cfmakeraw(&mut new_attrs);
    tcsetattr(terminal.writer(), SetArg::TCSAFLUSH, &new_attrs)?;
    
    Ok(Some(old_attrs))
}

/// Reset the terminal to the old arguments.
/// 
/// Only call this, if it's actually a tty.
fn reset_terminal(
    terminal: &mut Join<&mut Stdin, &mut Stdout>, attrs: Termios,
) -> std::result::Result<(), Errno> {
    tcsetattr(terminal.writer(), SetArg::TCSAFLUSH, &attrs)
}
