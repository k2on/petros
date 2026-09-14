//! What an app may change about its mutations, and what it may not.
//!
//! The log is permanent. Every entry ever written is replayed by every peer
//! forever, so the verbs and arguments an app declares are a promise to bytes
//! already on disk: a verb the log carries must still be understood, and an
//! argument it carries must still mean what it meant. Adding is always safe —
//! a payload that lacks a field decodes to that field's default — so the rule
//! is simply that the surface may grow and may not shrink or shift.
//!
//! That rule was a sentence in a contributing guide, which is the kind of rule
//! that holds until the afternoon somebody is busy. This turns it into a
//! comparison a machine makes: snapshot the schema a release shipped, compare
//! the next build against it, and refuse anything the log cannot survive.
//! `petros-log-compat` is the command; every app's flake runs it as a check.

use crate::{AppSchema, Ty};

/// A change the log cannot survive.
///
/// Each one names the verb and argument it is about, because the message is
/// read by somebody who has just made the change and needs to know which of
/// their edits was the problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Break {
    /// A verb the snapshot has and this build does not. Entries naming it are
    /// in the log and no longer have an `apply` to run.
    VerbRemoved { verb: String },
    /// An argument dropped from a verb that keeps its name. Entries carry the
    /// value; nothing reads it any more.
    ArgRemoved { verb: String, arg: String },
    /// An argument whose type changed under it. The bytes in the log were
    /// written as one thing and would now be read as another.
    ArgRetyped {
        verb: String,
        arg: String,
        was: Ty,
        now: Ty,
    },
}

impl core::fmt::Display for Break {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Break::VerbRemoved { verb } => write!(
                f,
                "the mutation `{verb}` is gone. The log still carries entries \
                 naming it, and every peer replays them — a verb may be added \
                 but never removed or renamed. Add the new one beside it."
            ),
            Break::ArgRemoved { verb, arg } => write!(
                f,
                "`{verb}` no longer takes `{arg}`. Entries in the log carry \
                 that argument; dropping it changes what they mean. Leave it \
                 in the signature, even if nothing reads it."
            ),
            Break::ArgRetyped {
                verb,
                arg,
                was,
                now,
            } => write!(
                f,
                "`{verb}.{arg}` was {} and is now {}. The bytes already \
                 written were encoded as the first and would be read as the \
                 second — add a differently named argument instead.",
                was.name(),
                now.name()
            ),
        }
    }
}

/// Every way `now` breaks the promise `was` made to the log.
///
/// Empty is the answer that lets a build through. Anything added — a new verb,
/// a new argument on an old verb — is absent from the result on purpose: that
/// is the one kind of change the format is designed to absorb, because a
/// payload without a field decodes to that field's default.
///
/// Argument *order* is not compared. Arguments travel in a map keyed by name,
/// so reordering them cannot change how an entry decodes; it changes the
/// generated foreign signature, which is a compile error at the call site
/// rather than a corruption on disk.
pub fn breaking_changes(was: &AppSchema, now: &AppSchema) -> Vec<Break> {
    let mut breaks = Vec::new();
    for old in &was.verbs {
        let Some(new) = now.verb(&old.name) else {
            breaks.push(Break::VerbRemoved {
                verb: old.name.clone(),
            });
            continue;
        };
        for arg in &old.args {
            match new.args.iter().find(|a| a.name == arg.name) {
                None => breaks.push(Break::ArgRemoved {
                    verb: old.name.clone(),
                    arg: arg.name.clone(),
                }),
                Some(found) if found.ty != arg.ty => breaks.push(Break::ArgRetyped {
                    verb: old.name.clone(),
                    arg: arg.name.clone(),
                    was: arg.ty,
                    now: found.ty,
                }),
                Some(_) => {}
            }
        }
    }
    breaks
}

/// The snapshot format: one verb per line, arguments as `name:Type`.
///
/// Text rather than JSON because this file's whole job is to be read in a
/// review — the diff is the change to the log's surface, and a reviewer should
/// be able to see "a line gained an argument" at a glance. It also keeps the
/// tool that writes it free of a serialisation dependency, which matters
/// because that tool shares a crate with the one in the edit loop.
pub fn to_text(schema: &AppSchema) -> String {
    let mut out = String::new();
    for verb in &schema.verbs {
        out.push_str(&verb.name);
        for arg in &verb.args {
            out.push(' ');
            out.push_str(&arg.name);
            out.push(':');
            out.push_str(arg.ty.name());
        }
        out.push('\n');
    }
    out
}

/// Read back what [`to_text`] wrote. Blank lines and `#` comments are skipped,
/// so the file can explain itself to whoever opens it.
pub fn from_text(text: &str) -> Result<AppSchema, String> {
    let mut verbs = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let name = parts
            .next()
            .ok_or_else(|| format!("line {}: empty", n + 1))?;
        let mut verb = crate::Verb::new(name);
        for part in parts {
            let (arg, ty) = part
                .split_once(':')
                .ok_or_else(|| format!("line {}: `{part}` is not `name:Type`", n + 1))?;
            verb = verb.arg(
                arg,
                Ty::parse(ty).map_err(|e| format!("line {}: {e}", n + 1))?,
            );
        }
        verbs.push(verb);
    }
    Ok(AppSchema::new(verbs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Verb;

    fn schema(verbs: impl IntoIterator<Item = Verb>) -> AppSchema {
        AppSchema::new(verbs)
    }

    fn add_song() -> Verb {
        Verb::new("AddSong")
            .arg("title", Ty::Text)
            .arg("artist", Ty::Text)
    }

    #[test]
    fn an_unchanged_schema_is_compatible() {
        let s = schema([add_song()]);
        assert!(breaking_changes(&s, &s).is_empty());
    }

    #[test]
    fn adding_a_verb_or_an_argument_is_compatible() {
        let was = schema([add_song()]);
        let now = schema([
            add_song().arg("duration_ms", Ty::Integer),
            Verb::new("AddSermon").arg("speaker", Ty::Text),
        ]);
        assert!(
            breaking_changes(&was, &now).is_empty(),
            "growing the surface is the whole point of the format"
        );
    }

    #[test]
    fn removing_a_verb_breaks() {
        let was = schema([add_song(), Verb::new("Favorite").arg("id", Ty::Id)]);
        let now = schema([add_song()]);
        assert_eq!(
            breaking_changes(&was, &now),
            vec![Break::VerbRemoved {
                verb: "Favorite".into()
            }]
        );
    }

    /// A rename is a removal and an addition, and the removal is what the log
    /// cannot survive — so it is reported as one rather than as a rename.
    #[test]
    fn renaming_a_verb_reads_as_removing_it() {
        let was = schema([add_song()]);
        let now = schema([Verb::new("AddTrack")
            .arg("title", Ty::Text)
            .arg("artist", Ty::Text)]);
        assert_eq!(
            breaking_changes(&was, &now),
            vec![Break::VerbRemoved {
                verb: "AddSong".into()
            }]
        );
    }

    #[test]
    fn removing_or_retyping_an_argument_breaks() {
        let was = schema([add_song()]);
        let now = schema([Verb::new("AddSong").arg("title", Ty::Integer)]);
        let breaks = breaking_changes(&was, &now);
        assert_eq!(
            breaks,
            vec![
                Break::ArgRetyped {
                    verb: "AddSong".into(),
                    arg: "title".into(),
                    was: Ty::Text,
                    now: Ty::Integer,
                },
                Break::ArgRemoved {
                    verb: "AddSong".into(),
                    arg: "artist".into(),
                },
            ]
        );
    }

    #[test]
    fn the_snapshot_round_trips() {
        let schema = schema([
            add_song().arg("id", Ty::Id).arg("done", Ty::Bool),
            Verb::new("FavoriteAll"),
        ]);
        let text = to_text(&schema);
        assert_eq!(
            text,
            "AddSong title:Text artist:Text id:Id done:Bool\nFavoriteAll\n"
        );
        assert_eq!(from_text(&text).unwrap(), schema);
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let text = "# what the log carries\n\nAddSong title:Text\n";
        assert_eq!(
            from_text(text).unwrap(),
            schema([Verb::new("AddSong").arg("title", Ty::Text)])
        );
    }

    /// Reordering changes the generated signature, not the bytes: arguments
    /// travel keyed by name.
    #[test]
    fn reordering_arguments_is_compatible() {
        let was = schema([add_song()]);
        let now = schema([Verb::new("AddSong")
            .arg("artist", Ty::Text)
            .arg("title", Ty::Text)]);
        assert!(breaking_changes(&was, &now).is_empty());
    }
}
