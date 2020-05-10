//! Execute Nix commands using a builder-pattern abstraction.
//! ```rust
//! extern crate lorri;
//! use lorri::nix;
//!
//! #[macro_use] extern crate serde_derive;
//! #[derive(Debug, Deserialize, PartialEq, Eq)]
//! struct Author {
//!     name: String,
//!     contributions: usize
//! }
//!
//! fn main() {
//!     let output: Result<Vec<Author>, _> = nix::CallOpts::expression(r#"
//!       { name }:
//!       {
//!         contributors = [
//!           { inherit name; contributions = 99; }
//!         ];
//!       }
//!     "#)
//!         .argstr("name", "Jill")
//!         .attribute("contributors")
//!         .value();
//!
//!     assert_eq!(
//!         output.unwrap(),
//!         vec![
//!             Author { name: "Jill".to_string(), contributions: 99 },
//!         ]
//!     );
//! }
//! ```

use crate::error::BuildError;
use crate::osstrlines;
use crossbeam_channel as chan;
use slog_scope::debug;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::thread;
use vec1::Vec1;

/// Execute Nix commands using a builder-pattern abstraction.
#[derive(Clone)]
pub struct CallOpts<'a> {
    input: Input<'a>,
    attribute: Option<String>,
    argstrs: HashMap<String, String>,
}

/// Which input to give nix.
#[derive(Clone)]
enum Input<'a> {
    /// A nix expression string.
    Expression(&'a str),
    /// A nix file.
    File(&'a Path),
}

/// A store path (generated by `nix-store --realize` from a .drv file).
#[derive(Hash, PartialEq, Eq, Clone, Debug)]
pub struct StorePath(PathBuf);

impl StorePath {
    /// Underlying `Path`.
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl From<&std::ffi::OsStr> for StorePath {
    fn from(s: &std::ffi::OsStr) -> StorePath {
        StorePath(PathBuf::from(s.to_owned()))
    }
}

impl From<std::ffi::OsString> for StorePath {
    fn from(s: std::ffi::OsString) -> StorePath {
        StorePath(PathBuf::from(s))
    }
}

/// Opaque type to keep a temporary GC root directory alive.
/// Once it is dropped, the GC root is removed.
#[derive(Debug)]
pub struct GcRootTempDir(tempfile::TempDir);

impl<'a> CallOpts<'a> {
    /// Create a CallOpts with the Nix expression `expr`.
    ///
    /// ```rust
    /// extern crate lorri;
    /// use lorri::nix;
    /// let output: Result<u8, _> = nix::CallOpts::expression("let x = 5; in x")
    ///     .value();
    /// assert_eq!(
    ///   output.unwrap(), 5
    /// );
    /// ```
    pub fn expression(expr: &str) -> CallOpts {
        CallOpts {
            input: Input::Expression(expr),
            attribute: None,
            argstrs: HashMap::new(),
        }
    }

    /// Create a CallOpts with the Nix file `nix_file`.
    pub fn file(nix_file: &Path) -> CallOpts {
        CallOpts {
            input: Input::File(nix_file),
            attribute: None,
            argstrs: HashMap::new(),
        }
    }

    /// Evaluate a sub attribute of the expression. Only supports one:
    /// calling attribute() multiple times is supported, but overwrites
    /// the previous attribute.
    ///
    ///
    /// ```rust
    /// extern crate lorri;
    /// use lorri::nix;
    /// let output: Result<u8, _> = nix::CallOpts::expression("let x = 5; in { a = x; }")
    ///     .attribute("a")
    ///     .value();
    /// assert_eq!(
    ///   output.unwrap(), 5
    /// );
    /// ```
    ///
    ///
    /// This is due to the following difficult to handle edge case of
    ///
    /// nix-instantiate --eval --strict --json -E '{ a = 1; b = 2; }' -A a -A b
    ///
    /// producing "12".
    pub fn attribute(&mut self, attr: &str) -> &mut Self {
        self.attribute = Some(attr.to_string());
        self
    }

    /// Specify an argument to the expression, where the argument's value
    /// is to be interpreted as a string.
    ///
    /// ```rust
    /// extern crate lorri;
    /// use lorri::nix;
    /// let output: Result<String, _> = nix::CallOpts::expression(r#"{ name }: "Hello, ${name}!""#)
    ///     .argstr("name", "Jill")
    ///     .value();
    /// assert_eq!(
    ///   output.unwrap(), "Hello, Jill!"
    /// );
    /// ```
    pub fn argstr(&mut self, name: &str, value: &str) -> &mut Self {
        self.argstrs.insert(name.to_string(), value.to_string());
        self
    }

    /// Evaluate the expression and parameters, and interpret as type T:
    ///
    /// ```rust
    /// extern crate lorri;
    /// use lorri::nix;
    ///
    /// #[macro_use] extern crate serde_derive;
    /// #[derive(Debug, Deserialize, PartialEq, Eq)]
    /// struct Author {
    ///     name: String,
    ///     contributions: usize
    /// }
    ///
    /// fn main() {
    ///     let output: Result<Vec<Author>, _> = nix::CallOpts::expression(r#"
    ///       { name }:
    ///       {
    ///         contributors = [
    ///           { inherit name; contributions = 99; }
    ///         ];
    ///       }
    ///     "#)
    ///         .argstr("name", "Jill")
    ///         .attribute("contributors")
    ///         .value();
    ///
    ///     assert_eq!(
    ///         output.unwrap(),
    ///         vec![
    ///             Author { name: "Jill".to_string(), contributions: 99 },
    ///         ]
    ///     );
    /// }
    /// ```
    pub fn value<T: 'static>(&self) -> Result<T, BuildError>
    where
        T: Send + serde::de::DeserializeOwned,
    {
        let mut cmd = Command::new("nix-instantiate");
        cmd.args(&["--eval", "--json", "--strict"]);
        cmd.args(self.command_arguments());
        self.execute(cmd, move |stdout_handle| {
            serde_json::from_reader::<_, T>(stdout_handle)
        })?
        .map_err(BuildError::io)
    }

    /// Build the expression and return a path to the build result:
    ///
    /// ```rust
    /// extern crate lorri;
    /// use lorri::nix;
    /// use std::path::{Path, PathBuf};
    /// # use std::env;
    /// # env::set_var("NIX_PATH", "nixpkgs=./nix/bogus-nixpkgs/");
    ///
    /// let (location, gc_root) = nix::CallOpts::expression(r#"
    ///             import <nixpkgs> {}
    /// "#)
    ///         .attribute("hello")
    ///         .path()
    ///         .unwrap()
    ///         ;
    ///
    /// let location = location.as_path().to_string_lossy().into_owned();
    /// println!("{:?}", location);
    /// assert!(location.contains("/nix/store"));
    /// assert!(location.contains("hello-"));
    /// drop(gc_root);
    /// ```
    ///
    /// `path` returns a lock to the GC roots created by the Nix call
    /// (`gc_root` in the example above). Until that is dropped,
    /// a Nix garbage collect will not remove the store paths created
    /// by `path()`.
    ///
    /// Note, `path()` returns an error if there are multiple store paths
    /// returned by Nix:
    ///
    /// ```rust
    /// extern crate lorri;
    /// use lorri::nix;
    /// use lorri::error::BuildError;
    /// use std::path::{Path, PathBuf};
    /// # use std::env;
    /// # env::set_var("NIX_PATH", "nixpkgs=./nix/bogus-nixpkgs/");
    ///
    /// let paths = nix::CallOpts::expression(r#"
    ///             { inherit (import <nixpkgs> {}) hello git; }
    /// "#)
    ///         .path();
    ///
    /// match paths {
    ///    Err(BuildError::Output { .. }) => {},
    ///    otherwise => panic!(otherwise)
    /// }
    /// ```
    pub fn path(&self) -> Result<(StorePath, GcRootTempDir), BuildError> {
        let (pathsv1, gc_root) = self.paths()?;
        let mut paths = pathsv1.into_vec();

        match (paths.pop(), paths.pop()) {
            // Exactly zero
            (None, _) => Err(BuildError::output(
                "expected exactly one build output, got zero".to_string(),
            )),

            // Exactly one
            (Some(path), None) => Ok((path, gc_root)),

            // More than one
            (Some(_), Some(_)) => Err(BuildError::output(
                "expected exactly one build output, got more".to_string(),
            )),
        }
    }

    /// Build the expression and return a list of paths to the build results.
    /// Like `.path()`, except it returns all store paths.
    ///
    /// ```rust
    /// extern crate lorri;
    /// use lorri::nix;
    /// use std::path::{Path, PathBuf};
    /// # use std::env;
    /// # env::set_var("NIX_PATH", "nixpkgs=./nix/bogus-nixpkgs/");
    ///
    /// let (paths, gc_root) = nix::CallOpts::expression(r#"
    ///             { inherit (import <nixpkgs> {}) hello git; }
    /// "#)
    ///         .paths()
    ///         .unwrap();
    /// let mut paths = paths
    ///         .into_iter()
    ///         .map(|path| { println!("{:?}", path); format!("{:?}", path) });
    /// assert!(paths.next().unwrap().contains("git-"));
    /// assert!(paths.next().unwrap().contains("hello-"));
    /// drop(gc_root);
    /// ```
    pub fn paths(&self) -> Result<(Vec1<StorePath>, GcRootTempDir), BuildError> {
        // TODO: temp_dir writes to /tmp by default, we should
        // create a wrapper using XDG_RUNTIME_DIR instead,
        // which is per-user and (on systemd systems) a tmpfs.
        let gc_root_dir = tempfile::TempDir::new()?;

        let mut cmd = Command::new("nix-build");

        // Create a gc root to the build output
        cmd.args(&[
            OsStr::new("--out-link"),
            gc_root_dir.path().join(Path::new("result")).as_os_str(),
        ]);

        cmd.args(self.command_arguments());

        debug!("nix-build"; "command" => ?cmd);

        let paths: Vec<StorePath> = self.execute(cmd, move |stdout_handle| {
            osstrlines::Lines::from(stdout_handle)
                .map(|line| line.map(StorePath::from))
                .collect::<Result<Vec<StorePath>, _>>()
        })??;

        if let Ok(vec1) = Vec1::try_from_vec(paths) {
            Ok((vec1, GcRootTempDir(gc_root_dir)))
        } else {
            Err(BuildError::output(
                "expected exactly one Nix output, got zero".to_string(),
            ))
        }
    }

    /// Execute a command (presumably a Nix command :)). stderr output
    /// is passed line-based to the CallOpts' stderr_line_tx receiver.
    /// Stdout is passed as a BufReader to `stdout_fn`.
    fn execute<T: 'static, S: 'static>(
        &self,
        mut cmd: Command,
        stdout_fn: S,
    ) -> Result<T, BuildError>
    where
        S: Send + Fn(std::io::BufReader<ChildStdout>) -> T,
        T: Send,
    {
        cmd.stderr(Stdio::piped());
        cmd.stdout(Stdio::piped());

        // 0. spawn the process
        let mut nix_proc = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => BuildError::spawn(&cmd, e),
            _ => BuildError::io(e),
        })?;

        // 1. spawn a stderr handling thread
        let (stderr_tx, stderr_rx) = chan::unbounded();
        let stderr_handle: ChildStderr = nix_proc.stderr.take().expect("failed to take stderr");
        let stderr_thread = thread::spawn(move || {
            let reader = osstrlines::Lines::from(BufReader::new(stderr_handle));
            for line in reader {
                stderr_tx
                    .send(line.unwrap())
                    .expect("Receiver for nix.rs hung up");
            }
        });

        // 2. spawn a stdout handling thread (?)
        let stdout_handle: ChildStdout = nix_proc.stdout.take().expect("failed to take stdout");
        let stdout_thread = thread::spawn(move || stdout_fn(BufReader::new(stdout_handle)));

        // 3. wait on the process
        let nix_proc_result = nix_proc.wait()?;

        // 4. join the stderr handler
        stderr_thread
            .join()
            .expect("stderr handling thread panicked");

        // 5. join the stdout handler
        let data_result = stdout_thread
            .join()
            .expect("stderr handling thread panicked");

        if !nix_proc_result.success() {
            Err(BuildError::exit(
                &cmd,
                nix_proc_result,
                stderr_rx.iter().collect::<Vec<_>>(),
            ))
        } else {
            Ok(data_result)
        }
    }

    /// Fetch common arguments passed to Nix's CLI, specifically
    /// the --expr expression, -A attribute, and --argstr values.
    fn command_arguments(&self) -> Vec<&OsStr> {
        let mut ret: Vec<&OsStr> = vec![];

        if let Some(ref attr) = self.attribute {
            ret.push(OsStr::new("-A"));
            ret.push(OsStr::new(attr));
        }

        for (name, value) in self.argstrs.iter() {
            ret.push(OsStr::new("--argstr"));
            ret.push(OsStr::new(name));
            ret.push(OsStr::new(value));
        }

        match self.input {
            Input::Expression(ref exp) => {
                ret.push(OsStr::new("--expr"));
                ret.push(OsStr::new(exp));
            }
            Input::File(ref fp) => {
                ret.push(OsStr::new("--"));
                ret.push(OsStr::new(fp));
            }
        }

        ret
    }
}

/// Possible error conditions encountered when executing Nix evaluation commands.
#[derive(Debug)]
pub enum EvaluationError {
    /// A system-level IO error occured while executing Nix.
    Io(std::io::Error),

    /// Nix commands not on PATH
    NixNotFound,

    /// Nix execution failed.
    ExecutionFailed(ExitStatus),

    /// The data returned from nix-instantiate did not match the
    /// data time you expect.
    Decoding(serde_json::Error),
}

impl From<std::io::Error> for EvaluationError {
    fn from(e: std::io::Error) -> EvaluationError {
        EvaluationError::Io(e)
    }
}

impl From<serde_json::Error> for EvaluationError {
    fn from(e: serde_json::Error) -> EvaluationError {
        EvaluationError::Decoding(e)
    }
}

impl From<ExitStatus> for EvaluationError {
    fn from(status: ExitStatus) -> EvaluationError {
        if status.success() {
            panic!(
                "Status is successful, but we're in error handling: {:#?}",
                status
            );
        }

        EvaluationError::ExecutionFailed(status)
    }
}

/// Possible error conditions encountered when executing a Nix build
/// and expecting a single result
#[derive(Debug)]
pub enum OnePathError {
    /// Too many paths were returned
    TooManyResults,

    /// Standard Build Error results
    Build(BuildError),
}

impl From<BuildError> for OnePathError {
    fn from(e: BuildError) -> OnePathError {
        OnePathError::Build(e)
    }
}

#[cfg(test)]
mod tests {
    use super::CallOpts;
    use std::ffi::OsStr;
    use std::path::Path;

    #[test]
    fn cmd_arguments_expression() {
        let mut nix = CallOpts::expression("my-cool-expression");
        nix.attribute("hello");
        nix.argstr("foo", "bar");

        let exp: Vec<&OsStr> = [
            "-A",
            "hello",
            "--argstr",
            "foo",
            "bar",
            "--expr",
            "my-cool-expression",
        ]
        .into_iter()
        .map(OsStr::new)
        .collect();
        assert_eq!(exp, nix.command_arguments());
    }

    #[test]
    fn cmd_arguments_test() {
        let mut nix2 = CallOpts::file(Path::new("/my-cool-file.nix"));
        nix2.attribute("hello");
        nix2.argstr("foo", "bar");
        let exp2: Vec<&OsStr> = [
            "-A",
            "hello",
            "--argstr",
            "foo",
            "bar",
            "--",
            "/my-cool-file.nix",
        ]
        .into_iter()
        .map(OsStr::new)
        .collect();
        assert_eq!(exp2, nix2.command_arguments());
    }
}
