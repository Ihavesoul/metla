//! Known-junk classification. Decided *during* the walk (the walker already
//! has sibling context in hand), stored as one byte on the node, and turned
//! into suggestions by the post-scan pass. Zero extra traversal cost.

/// Category of a directory the tool is confident is expendable.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum JunkCategory {
    NodeModules,
    RustTarget,
    Venv,
    Pods,
    DerivedData,
    DeviceSupport,
    XcodeArchives,
    Simulators,
    UserCaches,
    Gradle,
    Maven,
    NpmCache,
    Terraform,
    PythonBytecode,
    Trash,
}

impl JunkCategory {
    pub const ALL: [JunkCategory; 15] = [
        JunkCategory::NodeModules,
        JunkCategory::RustTarget,
        JunkCategory::Venv,
        JunkCategory::Pods,
        JunkCategory::DerivedData,
        JunkCategory::DeviceSupport,
        JunkCategory::XcodeArchives,
        JunkCategory::Simulators,
        JunkCategory::UserCaches,
        JunkCategory::Gradle,
        JunkCategory::Maven,
        JunkCategory::NpmCache,
        JunkCategory::Terraform,
        JunkCategory::PythonBytecode,
        JunkCategory::Trash,
    ];

    /// Short human label (used as a tag in the UI).
    pub fn as_str(self) -> &'static str {
        match self {
            JunkCategory::NodeModules => "node_modules",
            JunkCategory::RustTarget => "rust target",
            JunkCategory::Venv => "python venv",
            JunkCategory::Pods => "cocoapods",
            JunkCategory::DerivedData => "xcode derived data",
            JunkCategory::DeviceSupport => "xcode device support",
            JunkCategory::XcodeArchives => "xcode archives",
            JunkCategory::Simulators => "ios simulators",
            JunkCategory::UserCaches => "user caches",
            JunkCategory::Gradle => "gradle cache",
            JunkCategory::Maven => "maven repo",
            JunkCategory::NpmCache => "npm cache",
            JunkCategory::Terraform => "terraform cache",
            JunkCategory::PythonBytecode => "python bytecode",
            JunkCategory::Trash => "trash",
        }
    }

    /// `true` when the content is mechanically regenerable, so plain delete
    /// is the sensible recommendation. `false` → suggest archiving instead.
    pub fn regenerable(self) -> bool {
        matches!(
            self,
            JunkCategory::NodeModules
                | JunkCategory::RustTarget
                | JunkCategory::Venv
                | JunkCategory::Pods
                | JunkCategory::DerivedData
                | JunkCategory::DeviceSupport
                | JunkCategory::Gradle
                | JunkCategory::NpmCache
                | JunkCategory::Terraform
                | JunkCategory::PythonBytecode
                | JunkCategory::Trash
        )
    }

    /// Stable storage code for [`crate::arena::Node::junk`] (0 = none).
    pub fn as_code(self) -> u8 {
        1 + self as u8
    }

    /// Inverse of [`JunkCategory::as_code`].
    pub fn from_code(code: u8) -> Option<JunkCategory> {
        if code == 0 {
            return None;
        }
        JunkCategory::ALL.get((code - 1) as usize).copied()
    }
}

/// Context the walker accumulates for every directory it reads.
pub struct DirCtx<'a> {
    pub name: &'a [u8],
    /// A `Cargo.toml` / `Cargo.lock` sibling exists.
    pub sibling_cargo: bool,
    /// A `Podfile` sibling exists.
    pub sibling_podfile: bool,
    /// A `pyproject.toml` / `requirements.txt` sibling exists.
    pub sibling_python: bool,
    /// Some ancestor directory is named `Developer`.
    pub under_developer: bool,
    /// This directory is a direct child of `$HOME` (only way to be `Trash`).
    pub is_home_child: bool,
}

/// Classify a directory name in context. Names alone are trusted only when
/// they are unambiguous; generic ones (`target`, `venv`, `Pods`) require a
/// project-marker sibling.
pub fn classify(ctx: &DirCtx) -> Option<JunkCategory> {
    use JunkCategory::*;
    let cat = match ctx.name {
        b"node_modules" => NodeModules,
        b"target" if ctx.sibling_cargo => RustTarget,
        b".venv" => Venv,
        b"venv" if ctx.sibling_python => Venv,
        b"Pods" if ctx.sibling_podfile => Pods,
        b"DerivedData" if ctx.under_developer => DerivedData,
        b"iOS DeviceSupport" | b"watchOS DeviceSupport" if ctx.under_developer => DeviceSupport,
        b"Archives" if ctx.under_developer => XcodeArchives,
        b"CoreSimulator" if ctx.under_developer => Simulators,
        b"Caches" => UserCaches,
        b".gradle" => Gradle,
        b".m2" => Maven,
        b".npm" => NpmCache,
        b".terraform" => Terraform,
        b"__pycache__" | b".pytest_cache" | b".mypy_cache" | b".ipynb_checkpoints" => {
            PythonBytecode
        }
        b".Trash" if ctx.is_home_child => Trash,
        _ => return None,
    };
    Some(cat)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(name: &[u8]) -> DirCtx<'_> {
        DirCtx {
            name,
            sibling_cargo: false,
            sibling_podfile: false,
            sibling_python: false,
            under_developer: false,
            is_home_child: false,
        }
    }

    #[test]
    fn unambiguous_names_fire() {
        assert_eq!(
            classify(&ctx(b"node_modules")),
            Some(JunkCategory::NodeModules)
        );
        assert_eq!(classify(&ctx(b".venv")), Some(JunkCategory::Venv));
        assert_eq!(
            classify(&ctx(b"__pycache__")),
            Some(JunkCategory::PythonBytecode)
        );
    }

    #[test]
    fn ambiguous_names_need_context() {
        assert_eq!(classify(&ctx(b"target")), None);
        assert_eq!(classify(&ctx(b"venv")), None);
        assert_eq!(classify(&ctx(b"Pods")), None);
        assert_eq!(classify(&ctx(b".Trash")), None);

        let mut c = ctx(b"target");
        c.sibling_cargo = true;
        assert_eq!(classify(&c), Some(JunkCategory::RustTarget));

        let mut c = ctx(b"DerivedData");
        c.under_developer = true;
        assert_eq!(classify(&c), Some(JunkCategory::DerivedData));

        let mut c = ctx(b".Trash");
        c.is_home_child = true;
        assert_eq!(classify(&c), Some(JunkCategory::Trash));
    }

    #[test]
    fn codes_roundtrip() {
        for cat in JunkCategory::ALL {
            assert_eq!(JunkCategory::from_code(cat.as_code()), Some(cat));
        }
        assert_eq!(JunkCategory::from_code(0), None);
    }
}
