// Common test utilities

#[cfg(test)]
#[allow(dead_code)]
pub mod blog_showcase;
#[cfg(test)]
#[allow(dead_code)]
pub mod dormant_ssh;
#[cfg(test)]
#[allow(dead_code)]
pub mod fail_retirement_write_fs;
#[cfg(test)]
#[allow(dead_code)]
pub mod fake_lsp;
#[cfg(test)]
#[allow(dead_code)]
pub mod fixtures;
#[cfg(test)]
#[allow(dead_code)]
pub mod git_test_helper;
#[cfg(test)]
#[allow(dead_code)]
pub mod harness;
#[cfg(test)]
#[allow(dead_code)]
pub mod locale_lock;
#[cfg(test)]
#[allow(dead_code)]
pub mod scenario;
#[cfg(test)]
#[allow(dead_code)]
pub mod scrollbar;
#[cfg(test)]
#[allow(dead_code)]
pub mod timing;
#[cfg(test)]
#[allow(dead_code)]
pub mod tracing;
#[cfg(test)]
#[allow(dead_code)]
pub mod visual_testing;

#[cfg(test)]
static PATH_GUARD_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

#[cfg(test)]
pub struct PathGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
    previous_trusted_omp: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl PathGuard {
    pub fn prepend(path: &std::path::Path) -> Self {
        Self::prepend_impl(path, None)
    }

    pub fn prepend_with_trusted_omp(path: &std::path::Path, omp: &std::path::Path) -> Self {
        Self::prepend_impl(path, Some(omp))
    }

    fn prepend_impl(path: &std::path::Path, trusted_omp: Option<&std::path::Path>) -> Self {
        let lock = PATH_GUARD_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var_os("PATH");
        let previous_trusted_omp = std::env::var_os("FRESH_OMP_EXECUTABLE");
        let mut paths = vec![path.to_path_buf()];
        if let Some(value) = &previous {
            paths.extend(std::env::split_paths(value));
        }
        std::env::set_var(
            "PATH",
            std::env::join_paths(paths).expect("test PATH entries must be joinable"),
        );
        if let Some(omp) = trusted_omp {
            std::env::set_var("FRESH_OMP_EXECUTABLE", omp);
        }
        Self {
            _lock: lock,
            previous,
            previous_trusted_omp,
        }
    }
}

#[cfg(test)]
impl Drop for PathGuard {
    fn drop(&mut self) {
        if let Some(previous) = &self.previous {
            std::env::set_var("PATH", previous);
        } else {
            std::env::remove_var("PATH");
        }
        if let Some(previous) = &self.previous_trusted_omp {
            std::env::set_var("FRESH_OMP_EXECUTABLE", previous);
        } else {
            std::env::remove_var("FRESH_OMP_EXECUTABLE");
        }
    }
}

// Note: Visual regression tests write their own documentation files independently.
// No destructor needed - each test is self-contained and parallel-safe.
