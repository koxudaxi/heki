//! Module and name patterns shared by the contracts: the indexes that find the patterns a name
//! satisfies from the name itself (its literal parts, then one glob set), never by trying the
//! patterns one by one.

use crate::{NamePart, RuleConfigError};
use globset::{GlobSet, GlobSetBuilder};
use heki_env::glob::AnyGlob;
use rustc_hash::FxHashMap;

/// Parts of the matchers' texts ([`NamePart`]): the literal ones, with the matchers of each, and
/// the globs, with the matcher of each.
#[derive(Debug)]
pub(crate) struct PartIndex {
    literal: FxHashMap<String, Vec<usize>>,
    globs: GlobSet,
    glob_matchers: Vec<usize>,
}

impl PartIndex {
    /// Index `parts`, each with its matcher; their globs, each compiled already, may still be
    /// too many for one set (`GlobSet` under `context`).
    pub(crate) fn new<'p>(
        parts: impl Iterator<Item = (usize, &'p NamePart)>,
        context: &'static str,
    ) -> Result<Self, RuleConfigError> {
        let mut literal: FxHashMap<String, Vec<usize>> = FxHashMap::default();
        let mut globs = GlobSetBuilder::new();
        let mut glob_matchers = Vec::new();
        for (i, part) in parts {
            match part.glob() {
                Some(any) => {
                    for glob in any.globs() {
                        globs.add(glob.clone());
                        glob_matchers.push(i);
                    }
                }
                None => literal.entry(part.text().to_owned()).or_default().push(i),
            }
        }
        let globs = globs.build();
        Ok(Self {
            literal,
            globs: globs.map_err(|cause| RuleConfigError::GlobSet { context, cause })?,
            glob_matchers,
        })
    }

    /// Add to `out` the matchers whose part is `name` or a glob matching it.
    pub(crate) fn find(&self, name: &str, out: &mut Vec<usize>) {
        out.extend(self.literal.get(name).into_iter().flatten());
        out.extend(
            self.globs
                .matches(name)
                .into_iter()
                .map(|g| self.glob_matchers[g]),
        );
    }
}

/// The texts with a module part: by module part (each literal one, and each glob once), then by
/// qualname part, so that a function's module and qualname find them however many texts share a
/// module.
#[derive(Debug)]
pub(crate) struct ModuleIndex {
    literal: FxHashMap<String, PartIndex>,
    globs: GlobSet,
    /// The qualname parts of the texts under each glob of `globs`.
    glob_parts: Vec<PartIndex>,
}

impl ModuleIndex {
    /// Index the `(matcher, module part, qualname part)` of each text.
    pub(crate) fn new<'p>(
        parts: impl Iterator<Item = (usize, &'p NamePart, &'p NamePart)>,
        context: &'static str,
    ) -> Result<Self, RuleConfigError> {
        let mut literal: FxHashMap<&str, Vec<(usize, &NamePart)>> = FxHashMap::default();
        let mut globbed: Vec<(&AnyGlob, Vec<(usize, &NamePart)>)> = Vec::new();
        let mut glob_at: FxHashMap<&str, usize> = FxHashMap::default();
        for (i, module, qualname) in parts {
            match module.glob() {
                Some(glob) => {
                    let at = *glob_at.entry(module.text()).or_insert_with(|| {
                        globbed.push((glob, Vec::new()));
                        globbed.len() - 1
                    });
                    globbed[at].1.push((i, qualname));
                }
                None => literal
                    .entry(module.text())
                    .or_default()
                    .push((i, qualname)),
            }
        }
        let mut globs = GlobSetBuilder::new();
        let mut glob_parts = Vec::new();
        for (any, qualnames) in globbed {
            for glob in any.globs() {
                globs.add(glob.clone());
                glob_parts.push(PartIndex::new(qualnames.iter().copied(), context)?);
            }
        }
        let literal = literal
            .into_iter()
            .map(|(module, qualnames)| {
                Ok((
                    module.to_owned(),
                    PartIndex::new(qualnames.into_iter(), context)?,
                ))
            })
            .collect::<Result<_, RuleConfigError>>()?;
        let globs = globs.build();
        Ok(Self {
            literal,
            globs: globs.map_err(|cause| RuleConfigError::GlobSet { context, cause })?,
            glob_parts,
        })
    }

    /// Add to `out` the matchers whose module part names `module` and whose qualname part is
    /// `qualname` or a glob matching it.
    pub(crate) fn find(&self, module: &str, qualname: &str, out: &mut Vec<usize>) {
        if let Some(parts) = self.literal.get(module) {
            parts.find(qualname, out);
        }
        for g in self.globs.matches(module) {
            self.glob_parts[g].find(qualname, out);
        }
    }
}
