#![deny(missing_docs)]

//! This crate provides helper functions to activate and deactivate virtual environments.

use std::collections::HashMap;
use std::process::ExitStatus;
use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::shell::Shell;
use indexmap::IndexMap;
use rattler_conda_types::Platform;

const ENV_START_SEPERATOR: &str = "<=== RATTLER ENV START ===>";

/// Type of modification done to the `PATH` variable
#[derive(Default, Clone)]
pub enum PathModificationBehavior {
    /// Replaces the complete path variable with specified paths.
    #[default]
    Replace,
    /// Appends the new path variables to the path. E.g. '$PATH:/new/path'
    Append,
    /// Prepends the new path variables to the path. E.g. '/new/path:$PATH'
    Prepend,
}

/// A struct that contains the values of the environment variables that are relevant for the activation process.
/// The values are stored as strings. Currently, only the `PATH` and `CONDA_PREFIX` environment variables are used.
#[derive(Default, Clone)]
pub struct ActivationVariables {
    /// The value of the `CONDA_PREFIX` environment variable that contains the activated conda prefix path
    pub conda_prefix: Option<PathBuf>,

    /// The value of the `PATH` environment variable that contains the paths to the executables
    pub path: Option<Vec<PathBuf>>,

    /// The type of behavior of what should happen with the defined paths.
    pub path_modification_behavior: PathModificationBehavior,
}

impl ActivationVariables {
    /// Create a new `ActivationVariables` struct from the environment variables.
    pub fn from_env() -> Result<Self, std::env::VarError> {
        Ok(Self {
            conda_prefix: std::env::var("CONDA_PREFIX").ok().map(PathBuf::from),
            path: None,
            path_modification_behavior: PathModificationBehavior::Prepend,
        })
    }
}

/// A struct that holds values for the activation and deactivation
/// process of an environment, e.g. activation scripts to execute or environment variables to set.
#[derive(Debug)]
pub struct Activator<T: Shell> {
    /// The path to the root of the conda environment
    pub target_prefix: PathBuf,

    /// The type of shell that is being activated
    pub shell_type: T,

    /// Paths that need to be added to the PATH environment variable
    pub paths: Vec<PathBuf>,

    /// A list of scripts to run when activating the environment
    pub activation_scripts: Vec<PathBuf>,

    /// A list of scripts to run when deactivating the environment
    pub deactivation_scripts: Vec<PathBuf>,

    /// A list of environment variables to set when activating the environment
    pub env_vars: IndexMap<String, String>,

    /// The platform for which to generate the Activator
    pub platform: Platform,
}

/// Collect all script files that match a certain shell type from a given path.
/// The files are sorted by their filename.
/// If the path does not exist, an empty vector is returned.
/// If the path is not a directory, an error is returned.
///
/// # Arguments
///
/// * `path` - The path to the directory that contains the scripts
/// * `shell_type` - The type of shell that the scripts are for
///
/// # Returns
///
/// A vector of paths to the scripts
///
/// # Errors
///
/// If the path is not a directory, an error is returned.
fn collect_scripts<T: Shell>(path: &Path, shell_type: &T) -> Result<Vec<PathBuf>, std::io::Error> {
    // Check if path exists
    if !path.exists() {
        return Ok(vec![]);
    }

    let paths = fs::read_dir(path)?;

    let mut scripts = paths
        .into_iter()
        .filter_map(|r| r.ok())
        .map(|r| r.path())
        .filter(|path| shell_type.can_run_script(path))
        .collect::<Vec<_>>();

    scripts.sort();

    Ok(scripts)
}

/// Error that can occur when activating a conda environment
#[derive(thiserror::Error, Debug)]
pub enum ActivationError {
    /// An error that can occur when reading or writing files
    #[error(transparent)]
    IoError(#[from] std::io::Error),

    /// An error that can occur when parsing JSON
    #[error("Invalid json for environment vars: {0} in file {1:?}")]
    InvalidEnvVarFileJson(serde_json::Error, PathBuf),

    /// An error that can occur wiht malformed JSON when parsing files in the `env_vars.d` directory
    #[error("Malformed JSON: not a plain JSON object in file {file:?}")]
    InvalidEnvVarFileJsonNoObject {
        /// The path to the file that contains the malformed JSON
        file: PathBuf,
    },

    /// An error that can occur when `state` file is malformed
    #[error("Malformed JSON: file does not contain JSON object at key env_vars in file {file:?}")]
    InvalidEnvVarFileStateFile {
        /// The path to the file that contains the malformed JSON
        file: PathBuf,
    },

    /// An error that occurs when writing the activation script to a file fails
    #[error("Failed to write activation script to file {0}")]
    FailedToWriteActivationScript(#[from] std::fmt::Error),

    /// Failed to run the activation script
    #[error("Failed to run activation script (status: {status})")]
    FailedToRunActivationScript {
        /// The contents of the activation script that was run
        script: String,

        /// The stdout output of executing the script
        stdout: String,

        /// The stderr output of executing the script
        stderr: String,

        /// The error code of running the script
        status: ExitStatus,
    },
}

/// Collect all environment variables that are set in a conda environment.
/// The environment variables are collected from the `state` file and the `env_vars.d` directory in the given prefix
/// and are returned as a ordered map.
///
/// # Arguments
///
/// * `prefix` - The path to the root of the conda environment
///
/// # Returns
///
/// A map of environment variables
///
/// # Errors
///
/// If the `state` file or the `env_vars.d` directory cannot be read, an error is returned.
fn collect_env_vars(prefix: &Path) -> Result<IndexMap<String, String>, ActivationError> {
    let state_file = prefix.join("conda-meta/state");
    let pkg_env_var_dir = prefix.join("etc/conda/env_vars.d");
    let mut env_vars = IndexMap::new();

    if pkg_env_var_dir.exists() {
        let env_var_files = pkg_env_var_dir.read_dir()?;

        let mut env_var_files = env_var_files
            .into_iter()
            .filter_map(|r| r.ok())
            .map(|e| e.path())
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();

        // sort env var files to get a deterministic order
        env_var_files.sort();

        let env_var_json_files = env_var_files
            .iter()
            .map(|path| {
                fs::read_to_string(path)?
                    .parse::<serde_json::Value>()
                    .map_err(|e| ActivationError::InvalidEnvVarFileJson(e, path.to_path_buf()))
            })
            .collect::<Result<Vec<serde_json::Value>, ActivationError>>()?;

        for (env_var_json, env_var_file) in env_var_json_files.iter().zip(env_var_files.iter()) {
            let env_var_json = env_var_json.as_object().ok_or_else(|| {
                ActivationError::InvalidEnvVarFileJsonNoObject {
                    file: pkg_env_var_dir.to_path_buf(),
                }
            })?;

            for (key, value) in env_var_json {
                if let Some(value) = value.as_str() {
                    env_vars.insert(key.to_string(), value.to_string());
                } else {
                    tracing::warn!(
                        "WARNING: environment variable {key} has no string value (path: {env_var_file:?})");
                }
            }
        }
    }

    if state_file.exists() {
        let state_json = fs::read_to_string(&state_file)?;

        // load json but preserve the order of dicts - for this we use the serde preserve_order feature
        let state_json: serde_json::Value = serde_json::from_str(&state_json)
            .map_err(|e| ActivationError::InvalidEnvVarFileJson(e, state_file.to_path_buf()))?;

        let state_env_vars = state_json["env_vars"].as_object().ok_or_else(|| {
            ActivationError::InvalidEnvVarFileStateFile {
                file: state_file.to_path_buf(),
            }
        })?;

        for (key, value) in state_env_vars {
            if state_env_vars.contains_key(key) {
                tracing::warn!(
                    "WARNING: environment variable {key} already defined in packages (path: {state_file:?})");
            }

            if let Some(value) = value.as_str() {
                env_vars.insert(key.to_uppercase().to_string(), value.to_string());
            } else {
                tracing::warn!(
                    "WARNING: environment variable {key} has no string value (path: {state_file:?})");
            }
        }
    }
    Ok(env_vars)
}

/// Return a vector of path entries that are prefixed with the given path.
///
/// # Arguments
///
/// * `prefix` - The path to prefix the path entries with
/// * `operating_system` - The operating system that the path entries are for
///
/// # Returns
///
/// A vector of path entries
fn prefix_path_entries(prefix: &Path, platform: &Platform) -> Vec<PathBuf> {
    if platform.is_windows() {
        vec![
            prefix.to_path_buf(),
            prefix.join("Library/mingw-w64/bin"),
            prefix.join("Library/usr/bin"),
            prefix.join("Library/bin"),
            prefix.join("Scripts"),
            prefix.join("bin"),
        ]
    } else {
        vec![prefix.join("bin")]
    }
}

/// The result of a activation. It contains the activation script and the new path entries.
/// The activation script already sets the PATH environment variable, but for "environment stacking"
/// purposes it's useful to have the new path entries separately.
pub struct ActivationResult {
    /// The activation script that sets the environment variables, runs activation/deactivation scripts
    /// and sets the new PATH environment variable
    pub script: String,
    /// The new path entries that are added to the PATH environment variable
    pub path: Vec<PathBuf>,
}

impl<T: Shell + Clone> Activator<T> {
    /// Create a new activator for the given conda environment.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the root of the conda environment
    /// * `shell_type` - The shell type that the activator is for
    /// * `operating_system` - The operating system that the activator is for
    ///
    /// # Returns
    ///
    /// A new activator
    ///
    /// # Examples
    ///
    /// ```
    /// use rattler_shell::activation::Activator;
    /// use rattler_shell::shell;
    /// use rattler_conda_types::Platform;
    /// use std::path::PathBuf;
    ///
    /// let activator = Activator::from_path(&PathBuf::from("tests/fixtures/env_vars"), shell::Bash, Platform::Osx64).unwrap();
    /// assert_eq!(activator.paths.len(), 1);
    /// assert_eq!(activator.paths[0], PathBuf::from("tests/fixtures/env_vars/bin"));
    /// ```
    pub fn from_path(
        path: &Path,
        shell_type: T,
        platform: Platform,
    ) -> Result<Activator<T>, ActivationError> {
        let activation_scripts = collect_scripts(&path.join("etc/conda/activate.d"), &shell_type)?;

        let deactivation_scripts =
            collect_scripts(&path.join("etc/conda/deactivate.d"), &shell_type)?;

        let env_vars = collect_env_vars(path)?;

        let paths = prefix_path_entries(path, &platform);

        Ok(Activator {
            target_prefix: path.to_path_buf(),
            shell_type,
            paths,
            activation_scripts,
            deactivation_scripts,
            env_vars,
            platform,
        })
    }

    /// Create an activation script for a given shell and platform. This
    /// returns a tuple of the newly computed PATH variable and the activation script.
    pub fn activation(
        &self,
        variables: ActivationVariables,
    ) -> Result<ActivationResult, ActivationError> {
        let mut script = String::new();

        let mut path = variables.path.clone().unwrap_or_default();
        if let Some(conda_prefix) = variables.conda_prefix {
            let deactivate = Activator::from_path(
                Path::new(&conda_prefix),
                self.shell_type.clone(),
                self.platform,
            )?;

            for (key, _) in &deactivate.env_vars {
                self.shell_type
                    .unset_env_var(&mut script, key)
                    .map_err(ActivationError::FailedToWriteActivationScript)?;
            }

            for deactivation_script in &deactivate.deactivation_scripts {
                self.shell_type
                    .run_script(&mut script, deactivation_script)
                    .map_err(ActivationError::FailedToWriteActivationScript)?;
            }

            path.retain(|x| !deactivate.paths.contains(x));
        }

        // prepend new paths
        let path = [self.paths.clone(), path].concat();

        self.shell_type
            .set_path(
                &mut script,
                path.as_slice(),
                variables.path_modification_behavior,
                &self.platform,
            )
            .map_err(ActivationError::FailedToWriteActivationScript)?;

        // deliberately not taking care of `CONDA_SHLVL` or any other complications at this point
        self.shell_type
            .set_env_var(
                &mut script,
                "CONDA_PREFIX",
                &self.target_prefix.to_string_lossy(),
            )
            .map_err(ActivationError::FailedToWriteActivationScript)?;

        for (key, value) in &self.env_vars {
            self.shell_type
                .set_env_var(&mut script, key, value)
                .map_err(ActivationError::FailedToWriteActivationScript)?;
        }

        for activation_script in &self.activation_scripts {
            self.shell_type
                .run_script(&mut script, activation_script)
                .map_err(ActivationError::FailedToWriteActivationScript)?;
        }

        Ok(ActivationResult { script, path })
    }

    /// Runs the activation script and returns the environment variables changed in the environment
    /// after running the script.
    /// TODO: This only handles UTF-8 formatted strings..
    pub fn run_activation(
        &self,
        variables: ActivationVariables,
    ) -> Result<HashMap<String, String>, ActivationError> {
        let activation_script = self.activation(variables)?.script;

        // Create a script that starts by emitting all environment variables, then runs the
        // activation script followed by again emitting all environment variables. Any changes
        // should then become visible.
        let mut activation_detection_script = String::new();
        self.shell_type.env(&mut activation_detection_script)?;
        self.shell_type
            .echo(&mut activation_detection_script, ENV_START_SEPERATOR)?;
        activation_detection_script =
            format!("{}{}", &activation_detection_script, &activation_script);
        self.shell_type
            .echo(&mut activation_detection_script, ENV_START_SEPERATOR)?;
        self.shell_type.env(&mut activation_detection_script)?;

        // Create a temporary file that we can execute with our shell.
        let activation_script_dir = tempfile::TempDir::new()?;
        let activation_script_path = activation_script_dir
            .path()
            .join(format!("activation.{}", self.shell_type.extension()));
        fs::write(&activation_script_path, &activation_detection_script)?;

        // Get only the path to the temporary file
        let activation_result = self
            .shell_type
            .create_run_script_command(&activation_script_path)
            .output()?;

        if !activation_result.status.success() {
            return Err(ActivationError::FailedToRunActivationScript {
                script: activation_detection_script,
                stdout: String::from_utf8_lossy(&activation_result.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&activation_result.stderr).into_owned(),
                status: activation_result.status,
            });
        }

        let stdout = String::from_utf8_lossy(&activation_result.stdout);
        let (before_env, rest) = stdout
            .split_once(ENV_START_SEPERATOR)
            .unwrap_or(("", stdout.as_ref()));
        let (_, after_env) = rest.rsplit_once(ENV_START_SEPERATOR).unwrap_or(("", ""));

        // Parse both environments and find the difference
        let before_env = self.shell_type.parse_env(before_env);
        let after_env = self.shell_type.parse_env(after_env);

        // Find and return the differences
        Ok(after_env
            .into_iter()
            .filter(|(key, value)| before_env.get(key) != Some(value))
            // this happens on Windows for some reason
            // @SET "=C:=C:\Users\robostack\Programs\pixi"
            // @SET "=ExitCode=00000000"
            .filter(|(key, _)| !key.is_empty())
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use crate::shell;
    use std::collections::BTreeMap;
    use std::str::FromStr;

    use super::*;
    use tempdir::TempDir;

    #[cfg(unix)]
    use crate::activation::PathModificationBehavior;
    use crate::shell::ShellEnum;

    #[test]
    fn test_collect_scripts() {
        let tdir = TempDir::new("test").unwrap();

        let path = tdir.path().join("etc/conda/activate.d/");
        fs::create_dir_all(&path).unwrap();

        let script1 = path.join("script1.sh");
        let script2 = path.join("aaa.sh");
        let script3 = path.join("xxx.sh");

        fs::write(&script1, "").unwrap();
        fs::write(&script2, "").unwrap();
        fs::write(&script3, "").unwrap();

        let shell_type = shell::Bash;

        let scripts = collect_scripts(&path, &shell_type).unwrap();
        assert_eq!(scripts.len(), 3);
        assert_eq!(scripts[0], script2);
        assert_eq!(scripts[1], script1);
        assert_eq!(scripts[2], script3);

        let activator = Activator::from_path(tdir.path(), shell_type, Platform::Osx64).unwrap();
        assert_eq!(activator.activation_scripts.len(), 3);
        assert_eq!(activator.activation_scripts[0], script2);
        assert_eq!(activator.activation_scripts[1], script1);
        assert_eq!(activator.activation_scripts[2], script3);
    }

    #[test]
    fn test_collect_env_vars() {
        let tdir = TempDir::new("test").unwrap();
        let path = tdir.path().join("conda-meta/state");
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        let quotes = r#"{"env_vars": {"Hallo": "myval", "TEST": "itsatest", "AAA": "abcdef"}}"#;
        fs::write(&path, quotes).unwrap();

        let env_vars = collect_env_vars(tdir.path()).unwrap();
        assert_eq!(env_vars.len(), 3);

        assert_eq!(env_vars["HALLO"], "myval");
        assert_eq!(env_vars["TEST"], "itsatest");
        assert_eq!(env_vars["AAA"], "abcdef");
    }

    #[test]
    fn test_collect_env_vars_with_directory() {
        let tdir = TempDir::new("test").unwrap();
        let state_path = tdir.path().join("conda-meta/state");
        fs::create_dir_all(state_path.parent().unwrap()).unwrap();

        let content_pkg_1 = r#"{"VAR1": "someval", "TEST": "pkg1-test", "III": "super"}"#;
        let content_pkg_2 = r#"{"VAR1": "overwrite1", "TEST2": "pkg2-test"}"#;

        let env_var_d = tdir.path().join("etc/conda/env_vars.d");
        fs::create_dir_all(&env_var_d).expect("Could not create env vars directory");

        let pkg1 = env_var_d.join("pkg1.json");
        let pkg2 = env_var_d.join("pkg2.json");

        fs::write(&pkg1, content_pkg_1).expect("could not write file");
        fs::write(&pkg2, content_pkg_2).expect("could not write file");

        let quotes = r#"{"env_vars": {"Hallo": "myval", "TEST": "itsatest", "AAA": "abcdef"}}"#;
        fs::write(&state_path, quotes).unwrap();

        let env_vars = collect_env_vars(tdir.path()).expect("Could not load env vars");
        assert_eq!(env_vars.len(), 6);

        assert_eq!(env_vars["VAR1"], "overwrite1");
        assert_eq!(env_vars["TEST"], "itsatest");
        assert_eq!(env_vars["III"], "super");
        assert_eq!(env_vars["TEST2"], "pkg2-test");
        assert_eq!(env_vars["HALLO"], "myval");
        assert_eq!(env_vars["AAA"], "abcdef");

        // assert order of keys
        let mut keys = env_vars.keys();
        let key_vec = vec![
            "VAR1", // overwritten - should this be sorted down?
            "TEST", "III", "TEST2", "HALLO", "AAA",
        ];

        for key in key_vec {
            assert_eq!(keys.next().unwrap(), key);
        }
    }

    #[test]
    fn test_add_to_path() {
        let prefix = PathBuf::from_str("/opt/conda").unwrap();
        let new_paths = prefix_path_entries(&prefix, &Platform::Osx64);
        assert_eq!(new_paths.len(), 1);
    }

    #[cfg(unix)]
    fn create_temp_dir() -> TempDir {
        let tempdir = TempDir::new("test").unwrap();
        let path = tempdir.path().join("etc/conda/activate.d/");
        fs::create_dir_all(&path).unwrap();

        let script1 = path.join("script1.sh");

        fs::write(script1, "").unwrap();

        tempdir
    }

    #[cfg(unix)]
    fn get_script<T: Shell>(
        shell_type: T,
        path_modification_behavior: PathModificationBehavior,
    ) -> String
    where
        T: Clone,
    {
        let tdir = create_temp_dir();

        let activator = Activator::from_path(tdir.path(), shell_type, Platform::Osx64).unwrap();

        let result = activator
            .activation(ActivationVariables {
                conda_prefix: None,
                path: Some(vec![
                    PathBuf::from("/usr/bin"),
                    PathBuf::from("/bin"),
                    PathBuf::from("/usr/sbin"),
                    PathBuf::from("/sbin"),
                    PathBuf::from("/usr/local/bin"),
                ]),
                path_modification_behavior,
            })
            .unwrap();
        let prefix = tdir.path().to_str().unwrap();

        result.script.replace(prefix, "__PREFIX__")
    }

    #[test]
    #[cfg(unix)]
    fn test_activation_script_bash() {
        let script = get_script(shell::Bash, PathModificationBehavior::Append);
        insta::assert_snapshot!("test_activation_script_bash_append", script);
        let script = get_script(shell::Bash, PathModificationBehavior::Replace);
        insta::assert_snapshot!("test_activation_script_bash_replace", script);
        let script = get_script(shell::Bash, PathModificationBehavior::Prepend);
        insta::assert_snapshot!("test_activation_script_bash_prepend", script);
    }

    #[test]
    #[cfg(unix)]
    fn test_activation_script_zsh() {
        let script = get_script(shell::Zsh, PathModificationBehavior::Append);
        insta::assert_snapshot!(script);
    }

    #[test]
    #[cfg(unix)]
    fn test_activation_script_fish() {
        let script = get_script(shell::Fish, PathModificationBehavior::Append);
        insta::assert_snapshot!(script);
    }

    #[test]
    #[cfg(unix)]
    fn test_activation_script_powershell() {
        let script = get_script(
            shell::PowerShell::default(),
            PathModificationBehavior::Append,
        );
        insta::assert_snapshot!("test_activation_script_powershell_append", script);
        let script = get_script(
            shell::PowerShell::default(),
            PathModificationBehavior::Prepend,
        );
        insta::assert_snapshot!("test_activation_script_powershell_prepend", script);
        let script = get_script(
            shell::PowerShell::default(),
            PathModificationBehavior::Replace,
        );
        insta::assert_snapshot!("test_activation_script_powershell_replace", script);
    }

    #[test]
    #[cfg(unix)]
    fn test_activation_script_cmd() {
        let script = get_script(shell::CmdExe, PathModificationBehavior::Append);
        insta::assert_snapshot!("test_activation_script_cmd_append", script);
        let script = get_script(shell::CmdExe, PathModificationBehavior::Replace);
        insta::assert_snapshot!("test_activation_script_cmd_replace", script);
        let script = get_script(shell::CmdExe, PathModificationBehavior::Prepend);
        insta::assert_snapshot!("test_activation_script_cmd_prepend", script);
    }

    #[test]
    #[cfg(unix)]
    fn test_activation_script_xonsh() {
        let script = get_script(shell::Xonsh, PathModificationBehavior::Append);
        insta::assert_snapshot!(script);
    }

    fn test_run_activation(shell: ShellEnum) {
        let environment_dir = tempfile::TempDir::new().unwrap();

        // Write some environment variables to the `conda-meta/state` folder.
        let state_path = environment_dir.path().join("conda-meta/state");
        fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        let quotes = r#"{"env_vars": {"STATE": "Hello, world!"}}"#;
        fs::write(&state_path, quotes).unwrap();

        // Write package specific environment variables
        let content_pkg_1 = r#"{"PKG1": "Hello, world!"}"#;
        let content_pkg_2 = r#"{"PKG2": "Hello, world!"}"#;

        let env_var_d = environment_dir.path().join("etc/conda/env_vars.d");
        fs::create_dir_all(&env_var_d).expect("Could not create env vars directory");

        let pkg1 = env_var_d.join("pkg1.json");
        let pkg2 = env_var_d.join("pkg2.json");

        fs::write(&pkg1, content_pkg_1).expect("could not write file");
        fs::write(&pkg2, content_pkg_2).expect("could not write file");

        // Write a script that emits a random environment variable via a shell
        let mut activation_script = String::new();
        shell
            .set_env_var(&mut activation_script, "SCRIPT_ENV", "Hello, world!")
            .unwrap();

        let activation_script_dir = environment_dir.path().join("etc/conda/activate.d");
        fs::create_dir_all(&activation_script_dir).unwrap();

        fs::write(
            activation_script_dir.join(format!("pkg1.{}", shell.extension())),
            activation_script,
        )
        .unwrap();

        // Create an activator for the environment
        let activator =
            Activator::from_path(environment_dir.path(), shell.clone(), Platform::current())
                .unwrap();
        let activation_env = activator
            .run_activation(ActivationVariables::default())
            .unwrap();

        // Diff with the current environment
        let current_env = std::env::vars().collect::<HashMap<_, _>>();
        let mut env_diff = activation_env
            .into_iter()
            .filter(|(key, value)| current_env.get(key) != Some(value))
            .collect::<BTreeMap<_, _>>();

        // Remove system specific environment variables.
        env_diff.remove("CONDA_PREFIX");
        env_diff.remove("Path");
        env_diff.remove("PATH");

        insta::assert_yaml_snapshot!(shell.executable(), env_diff);
    }

    #[test]
    #[cfg(windows)]
    fn test_run_activation_powershell() {
        test_run_activation(crate::shell::PowerShell::default().into())
    }

    #[test]
    #[cfg(windows)]
    fn test_run_activation_cmd() {
        test_run_activation(crate::shell::CmdExe::default().into())
    }

    #[test]
    #[cfg(unix)]
    fn test_run_activation_bash() {
        test_run_activation(crate::shell::Bash::default().into())
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_run_activation_zsh() {
        test_run_activation(crate::shell::Zsh::default().into())
    }

    #[test]
    #[cfg(unix)]
    #[ignore]
    fn test_run_activation_fish() {
        test_run_activation(crate::shell::Fish::default().into())
    }

    #[test]
    #[cfg(unix)]
    #[ignore]
    fn test_run_activation_xonsh() {
        test_run_activation(crate::shell::Xonsh::default().into())
    }
}
