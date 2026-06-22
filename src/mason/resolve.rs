use crate::error::{Error, Result};
use crate::mason::install::{resolve_cached_program, resolve_or_install_program};
use crate::mason::link::{is_command_runnable, is_command_runnable_path, rewrite_program};
use crate::mason::registry::MasonRegistry;
use crate::runtime_state::{RuntimeState, default_runtime_state_root};
use crate::suggest::SuggestedLanguage;

pub fn resolve_detect_suggestions(
    suggestions: &[SuggestedLanguage],
    download: bool,
) -> Result<Vec<SuggestedLanguage>> {
    let state = default_runtime_state_root().ok().map(RuntimeState::new);
    let cached_registry = state.as_ref().and_then(MasonRegistry::load_cached);
    let mut install_registry = None;
    let mut resolved = Vec::new();
    let mut errors = Vec::new();

    for suggestion in suggestions {
        if let Some(suggestion) = resolve_suggestion_from_path_or_cache(
            suggestion,
            state.as_ref(),
            cached_registry.as_ref(),
        )? {
            resolved.push(suggestion);
            continue;
        }

        if !download {
            continue;
        }

        match install_suggestion(suggestion, state.as_ref(), &mut install_registry) {
            Ok(suggestion) => resolved.push(suggestion),
            Err(error) => errors.push(error),
        }
    }

    if !resolved.is_empty() {
        for error in errors {
            eprintln!("warning: {error}");
        }
        return Ok(resolved);
    }

    match errors.len() {
        0 => Ok(Vec::new()),
        1 => Err(errors.remove(0)),
        _ => Err(Error::unexpected(
            errors
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )),
    }
}

fn resolve_suggestion_from_path_or_cache(
    suggestion: &SuggestedLanguage,
    state: Option<&RuntimeState>,
    registry: Option<&MasonRegistry>,
) -> Result<Option<SuggestedLanguage>> {
    let Some(program) = suggestion.command.first() else {
        return Err(Error::unexpected(format!(
            "selected LSP server {} has an empty command",
            suggestion.server
        )));
    };

    if is_command_runnable(program) {
        return Ok(Some(suggestion.clone()));
    }

    // Check project-local node_modules/.bin/ — Node.js-based LSP servers
    // (e.g. typescript-language-server) are commonly installed there rather
    // than globally. Walk upward from workspace_root, stopping at filesystem
    // boundaries or after a reasonable depth.
    if let Some(local_path) = find_in_node_modules_bin(&suggestion.workspace_root, program) {
        return Ok(Some(rewrite_program(suggestion, &local_path)));
    }

    if program.contains(std::path::MAIN_SEPARATOR) {
        return Ok(None);
    }

    let Some(state) = state else {
        return Ok(None);
    };
    let Some(registry) = registry else {
        return Ok(None);
    };
    let Some(package) =
        registry.package_for_detected(&suggestion.config_id, &suggestion.server, program)
    else {
        return Ok(None);
    };

    let Ok(Some(executable_path)) = resolve_cached_program(state, package, program) else {
        return Ok(None);
    };

    Ok(Some(rewrite_program(suggestion, &executable_path)))
}

fn install_suggestion(
    suggestion: &SuggestedLanguage,
    state: Option<&RuntimeState>,
    registry: &mut Option<MasonRegistry>,
) -> Result<SuggestedLanguage> {
    let Some(program) = suggestion.command.first() else {
        return Err(Error::unexpected(format!(
            "selected LSP server {} has an empty command",
            suggestion.server
        )));
    };
    if program.contains(std::path::MAIN_SEPARATOR) {
        return Err(Error::missing_executable(format!(
            "configured LSP server executable `{program}` was not found"
        )));
    }

    let Some(state) = state else {
        return Err(Error::unexpected(
            "cannot install LSP servers automatically because $HOME is not set",
        ));
    };
    let registry = registry.get_or_insert(MasonRegistry::load(state)?);
    let Some(package) =
        registry.package_for_detected(&suggestion.config_id, &suggestion.server, program)
    else {
        return Err(Error::unexpected(format!(
            "no Mason install recipe is available for detected server {}",
            suggestion.server
        )));
    };
    let executable_path = resolve_or_install_program(state, package, program)?;

    Ok(rewrite_program(suggestion, &executable_path))
}

/// Walk upward from `workspace_root` looking for `node_modules/.bin/<program>`.
/// This covers the common Node.js layout where language servers are installed
/// as project dependencies (e.g. typescript-language-server in a monorepo).
/// Stops at filesystem boundaries or after a reasonable depth (10 levels).
fn find_in_node_modules_bin(
    workspace_root: &std::path::Path,
    program: &str,
) -> Option<std::path::PathBuf> {
    const MAX_DEPTH: usize = 10;
    for (i, dir) in workspace_root.ancestors().enumerate() {
        if i >= MAX_DEPTH {
            break;
        }
        let candidate = dir.join("node_modules").join(".bin").join(program);
        if is_command_runnable_path(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::resolve_detect_suggestions;
    use crate::suggest::SuggestedLanguage;
    use crate::test_support::{
        TestDir, env_var, jdtls_package, make_executable, pyright_package, runtime_state_in_home,
        suggested_language, with_env_vars, write_registry,
    };
    use std::fs;

    fn prepare_registry_test_home(
        package_name: &str,
        packages: &[crate::mason::registry::MasonPackage],
    ) -> (
        TestDir,
        std::path::PathBuf,
        crate::runtime_state::RuntimeState,
    ) {
        let dir = TestDir::new("mason-resolve");
        let home = dir.path().join("home");
        let state = runtime_state_in_home(&home);
        state.ensure_dirs().expect("state dirs should be created");
        write_registry(&state, packages);
        let package_dir = state.package_dir(package_name);
        (dir, package_dir, state)
    }

    #[cfg(unix)]
    #[test]
    fn prefers_cached_direct_binary_when_path_misses() {
        let (dir, package_dir, _state) =
            prepare_registry_test_home("pyright", &[pyright_package()]);
        let home = dir.path().join("home");
        let cached = package_dir.join("node_modules/.bin/pyright-langserver");
        fs::create_dir_all(cached.parent().expect("parent should exist"))
            .expect("parent dirs should be created");
        fs::write(&cached, b"stub\n").expect("cached binary should be written");
        make_executable(&cached);

        let resolved = with_env_vars(
            &[env_var("HOME", &home), env_var("PATH", "/nonexistent")],
            || {
                resolve_detect_suggestions(
                    &[suggested_language(
                        "pyright-langserver",
                        "pyright",
                        "pyright",
                        "python",
                    )],
                    false,
                )
                .expect("resolution should succeed")
            },
        );

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].command[0], cached.display().to_string());
    }

    #[cfg(unix)]
    #[test]
    fn prefers_cached_wrapper_when_path_misses() {
        let (dir, package_dir, state) = prepare_registry_test_home("jdtls", &[jdtls_package()]);
        let home = dir.path().join("home");
        let target = package_dir.join("bin/jdtls");
        fs::create_dir_all(target.parent().expect("parent should exist"))
            .expect("parent dirs should be created");
        fs::write(&target, b"print('ok')\n").expect("target should be written");
        let launcher = state.bin_dir().join("jdtls");
        fs::write(&launcher, b"stub\n").expect("launcher should be written");
        make_executable(&launcher);
        let runtime_dir = dir.path().join("bin");
        fs::create_dir_all(&runtime_dir).expect("runtime dir should be created");
        let python = runtime_dir.join("python3");
        fs::write(&python, b"stub\n").expect("runtime should be written");
        make_executable(&python);

        let resolved = with_env_vars(
            &[
                env_var("HOME", &home),
                env_var("PATH", runtime_dir.display().to_string()),
            ],
            || {
                resolve_detect_suggestions(
                    &[suggested_language("jdtls", "jdtls", "jdtls", "python")],
                    false,
                )
                .expect("resolution should succeed")
            },
        );

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].command[0], launcher.display().to_string());
    }

    #[cfg(unix)]
    #[test]
    fn skips_server_when_not_in_path_or_cache() {
        let dir = TestDir::new("mason-resolve");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("home dir should be created");

        let resolved = with_env_vars(
            &[env_var("HOME", &home), env_var("PATH", "/nonexistent")],
            || {
                resolve_detect_suggestions(
                    &[suggested_language(
                        "pyright-langserver",
                        "pyright",
                        "pyright",
                        "python",
                    )],
                    false,
                )
                .expect("resolution should succeed")
            },
        );

        assert!(resolved.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn finds_server_in_project_node_modules_bin() {
        let dir = TestDir::new("mason-resolve");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("home dir should be created");

        let workspace = dir.path().join("project");
        let bin = workspace.join("node_modules/.bin/typescript-language-server");
        fs::create_dir_all(bin.parent().expect("parent should exist"))
            .expect("node_modules/.bin dirs should be created");
        fs::write(&bin, b"stub\n").expect("binary should be written");
        make_executable(&bin);

        let resolved = with_env_vars(
            &[env_var("HOME", &home), env_var("PATH", "/nonexistent")],
            || {
                resolve_detect_suggestions(
                    &[SuggestedLanguage {
                        config_id: "ts".to_string(),
                        languages: vec!["typescript".to_string()],
                        server: "typescript-language-server".to_string(),
                        command: vec![
                            "typescript-language-server".to_string(),
                            "--stdio".to_string(),
                        ],
                        workspace_root: workspace.clone(),
                        wait_for_index: false,
                    }],
                    false,
                )
                .expect("resolution should succeed")
            },
        );

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].command[0], bin.display().to_string());
    }

    #[cfg(unix)]
    #[test]
    fn finds_server_in_parent_node_modules_bin() {
        let dir = TestDir::new("mason-resolve");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("home dir should be created");

        // Monorepo: binary is in root node_modules/.bin, workspace is a subdirectory
        let root = dir.path().join("monorepo");
        let workspace = root.join("packages/app");
        let bin = root.join("node_modules/.bin/typescript-language-server");
        fs::create_dir_all(bin.parent().expect("parent should exist"))
            .expect("node_modules/.bin dirs should be created");
        fs::write(&bin, b"stub\n").expect("binary should be written");
        make_executable(&bin);

        let resolved = with_env_vars(
            &[env_var("HOME", &home), env_var("PATH", "/nonexistent")],
            || {
                resolve_detect_suggestions(
                    &[SuggestedLanguage {
                        config_id: "ts".to_string(),
                        languages: vec!["typescript".to_string()],
                        server: "typescript-language-server".to_string(),
                        command: vec![
                            "typescript-language-server".to_string(),
                            "--stdio".to_string(),
                        ],
                        workspace_root: workspace.clone(),
                        wait_for_index: false,
                    }],
                    false,
                )
                .expect("resolution should succeed")
            },
        );

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].command[0], bin.display().to_string());
    }

    #[cfg(unix)]
    #[test]
    fn treats_corrupted_cache_as_missing() {
        let dir = TestDir::new("mason-resolve");
        let home = dir.path().join("home");
        let state = runtime_state_in_home(&home);
        state.ensure_dirs().expect("state dirs should be created");
        fs::write(state.registry_json_path(), b"not json")
            .expect("corrupted registry should be written");

        let resolved = with_env_vars(
            &[env_var("HOME", &home), env_var("PATH", "/nonexistent")],
            || {
                resolve_detect_suggestions(
                    &[suggested_language(
                        "pyright-langserver",
                        "pyright",
                        "pyright",
                        "python",
                    )],
                    false,
                )
                .expect("resolution should succeed")
            },
        );

        assert!(resolved.is_empty());
    }
}
