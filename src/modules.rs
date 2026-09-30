//! Static image layout, in 4 KiB pages, shared with the loader build script.
use std::{collections::BTreeSet, fs, path::Path};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Modules {
    pub module: Vec<Module>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Module {
    pub name: String,
    pub offset: usize,
    pub size: usize,
}

impl Module {
    fn validate(&self, edge: usize) -> Result<()> {
        ensure!(!self.name.is_empty(), "module name must not be empty");
        ensure!(self.name.len() <= 20, "module name exceeds 20 bytes");
        ensure!(
            self.name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-'),
            "invalid module name"
        );
        ensure!(self.size > 0, "modules need nonzero page ranges");
        ensure!(
            self.offset >= edge,
            "modules need non-overlapping page ranges"
        );
        ensure!(
            self.name != "supervisor" || self.size <= 0xdf,
            "supervisor overlaps the allocator's virtual range"
        );
        Ok(())
    }
}

impl Modules {
    pub fn read(path: &Path) -> Result<Self> {
        let config: Self = toml::from_str(&fs::read_to_string(path)?)
            .with_context(|| format!("parse {}", path.display()))?;
        let mut end = 0;
        let mut names = BTreeSet::new();
        for module in &config.module {
            module
                .validate(end)
                .with_context(|| format!("module {:?}", module.name))?;
            ensure!(
                names.insert(&module.name),
                "duplicate module name: {}",
                module.name
            );
            end = module
                .offset
                .checked_add(module.size)
                .context("module range overflow")?;
            // The loader currently identity maps the image within one 2 MiB leaf table.
            ensure!(end <= 0x1e0, "modules and boot DTB must fit in 2 MiB");
        }
        ensure!(
            config
                .module
                .first()
                .is_some_and(|m| m.name == "loader" && m.offset == 0),
            "loader must be the first module, at page zero"
        );
        for name in ["supervisor", "system", "system-test"] {
            ensure!(names.contains(&name.to_owned()), "missing module {name}");
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_overlapping_and_zero_sized_modules() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("2-S/modules.toml");
        fs::create_dir(directory.path().join("2-S")).unwrap();
        for (offset, size) in [(0, 1), (1, 0)] {
            fs::write(
                &path,
                format!(
                    "[[module]]\nname='loader'\noffset=0\nsize=1\n\
                 [[module]]\nname='supervisor'\noffset={offset}\nsize={size}\n\
                 [[module]]\nname='system'\noffset=2\nsize=1\n\
                 [[module]]\nname='system-test'\noffset=3\nsize=1\n"
                ),
            )
            .unwrap();
            assert!(Modules::read(&path).is_err());
        }
    }
}
