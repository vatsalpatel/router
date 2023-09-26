use std::fmt::Write as _;

use itertools::Itertools;
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::Value;

use crate::error::ConfigurationError;
use crate::json_ext::serde_json_insert;
use crate::json_ext::serde_json_iterate_path_mut;
use crate::json_ext::Path;
use crate::json_ext::PathElement;

#[derive(RustEmbed)]
#[folder = "src/configuration/migrations"]
struct Asset;

#[derive(Deserialize, buildstructor::Builder)]
struct Migration {
    description: String,
    actions: Vec<Action>,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Action {
    Add {
        path: String,
        name: String,
        value: Value,
    },
    Delete {
        path: String,
    },
    Copy {
        from: String,
        to: String,
    },
    Move {
        from: String,
        to: String,
    },
    Change {
        path: String,
        from: Value,
        to: Value,
    },
    Rename {
        path: String,
        name: String,
        to: String,
    },
}

const REMOVAL_VALUE: &str = "__PLEASE_DELETE_ME";

pub(crate) fn upgrade_configuration(
    config: &serde_json::Value,
    log_warnings: bool,
) -> Result<serde_json::Value, super::ConfigurationError> {
    // Transformers are loaded from a file and applied in order
    let migrations: Vec<Migration> = Asset::iter()
        .sorted()
        .filter(|filename| filename.ends_with(".yaml"))
        .map(|filename| Asset::get(&filename).expect("migration must exist").data)
        .map(|data| serde_yaml::from_slice(&data).expect("migration must be valid"))
        .collect();

    let mut config = config.clone();

    let mut effective_migrations = Vec::new();
    for migration in &migrations {
        let new_config = apply_migration(&config, migration)?;

        // If the config has been modified by the migration then let the user know
        if new_config != config {
            effective_migrations.push(migration);
        }

        // Get ready for the next migration
        config = new_config;
    }
    if !effective_migrations.is_empty() && log_warnings {
        tracing::warn!("router configuration contains deprecated options: \n\n{}\n\nThese will become errors in the future. Run `router config upgrade <path_to_router.yaml>` to see a suggested upgraded configuration.", effective_migrations.iter().enumerate().map(|(idx, m)|format!("  {}. {}", idx + 1, m.description)).join("\n\n"));
    }
    Ok(config)
}

fn apply_migration(config: &Value, migration: &Migration) -> Result<Value, ConfigurationError> {
    let mut new_config = config.to_owned();

    for action in &migration.actions {
        match action {
            Action::Add { path, name, value } => {
                let mut path = Path::from_json_path(path);
                let mut found_parent = false;

                serde_json_iterate_path_mut(
                    &mut Path::default(),
                    &path.0,
                    &mut new_config,
                    &mut |_path, _value| {
                        found_parent = true;
                    },
                );

                if found_parent {
                    path.push(PathElement::Key(name.to_owned()));

                    let mut found_element = false;

                    serde_json_iterate_path_mut(
                        &mut Path::default(),
                        &path.0,
                        &mut new_config,
                        &mut |_path, _value| {
                            found_element = true;
                        },
                    );
                    if !found_element {
                        let _ = serde_json_insert(&mut new_config, &path, value.clone());
                    }
                }
            }
            Action::Delete { path } => {
                let path = Path::from_json_path(path);

                serde_json_iterate_path_mut(
                    &mut Path::default(),
                    &path.0,
                    &mut new_config,
                    &mut |_path, value| {
                        *value = Value::Null;
                    },
                );
            }
            Action::Copy { from, to } => {
                let from_path = Path::from_json_path(from);

                let mut matched = None;

                serde_json_iterate_path_mut(
                    &mut Path::default(),
                    &from_path.0,
                    &mut new_config,
                    &mut |_path, value| {
                        if matched.is_none() {
                            matched = Some(value.clone());
                        }
                    },
                );

                let to_path = Path::from_json_path(to);

                if let Some(from_value) = matched.take() {
                    let _ = serde_json_insert(&mut new_config, &to_path, from_value);
                }
            }
            Action::Move { from, to } => {
                let from_path: Path = Path::from_json_path(from);

                let mut matched = None;

                serde_json_iterate_path_mut(
                    &mut Path::default(),
                    &from_path.0,
                    &mut new_config,
                    &mut |_path, value| {
                        if matched.is_none() {
                            matched = Some(std::mem::replace(value, Value::Null));
                        }
                    },
                );

                let to_path = Path::from_json_path(to);

                if let Some(from_value) = matched.take() {
                    let _ = serde_json_insert(&mut new_config, &to_path, from_value);
                }
            }
            Action::Change { path, from, to } => {
                let from_path: Path = Path::from_json_path(path);

                serde_json_iterate_path_mut(
                    &mut Path::default(),
                    &from_path.0,
                    &mut new_config,
                    &mut |_path, value| {
                        if value == from {
                            *value = to.to_owned();
                        }
                    },
                );
            }
            Action::Rename { path, name, to } => {
                let path: Path = Path::from_json_path(path);

                serde_json_iterate_path_mut(
                    &mut Path::default(),
                    &path.0,
                    &mut new_config,
                    &mut |_path, value| {
                        if let Some(o) = value.as_object_mut() {
                            if let Some(v) = o.remove(name) {
                                o.insert(to.to_string(), v);
                            }
                        }
                    },
                );
            }
        }
    }

    // Now we need to clean up elements that should be deleted.
    cleanup(&mut new_config);

    Ok(new_config)
}

//struct ChangeAction
pub(crate) fn generate_upgrade(config: &str, diff: bool) -> Result<String, ConfigurationError> {
    let parsed_config =
        serde_yaml::from_str(config).map_err(|e| ConfigurationError::MigrationFailure {
            error: e.to_string(),
        })?;
    let upgraded_config = upgrade_configuration(&parsed_config, true).map_err(|e| {
        ConfigurationError::MigrationFailure {
            error: e.to_string(),
        }
    })?;
    let upgraded_config = serde_yaml::to_string(&upgraded_config).map_err(|e| {
        ConfigurationError::MigrationFailure {
            error: e.to_string(),
        }
    })?;
    generate_upgrade_output(config, &upgraded_config, diff)
}

pub(crate) fn generate_upgrade_output(
    config: &str,
    upgraded_config: &str,
    diff: bool,
) -> Result<String, ConfigurationError> {
    // serde doesn't deal with whitespace and comments, these are lost in the upgrade process, so instead we try and preserve this in the diff.
    // It's not ideal, and ideally the upgrade process should work on a DOM that is not serde, but for now we just make a best effort to preserve comments and whitespace.
    // There absolutely are issues where comments will get stripped, but the output should be `correct`.
    let mut output = String::new();

    let diff_result = diff::lines(config, upgraded_config);

    for diff_line in diff_result {
        match diff_line {
            diff::Result::Left(l) => {
                let trimmed = l.trim();
                if !trimmed.starts_with('#') && !trimmed.is_empty() {
                    if diff {
                        writeln!(output, "-{l}").expect("write will never fail");
                    }
                } else if diff {
                    writeln!(output, " {l}").expect("write will never fail");
                } else {
                    writeln!(output, "{l}").expect("write will never fail");
                }
            }
            diff::Result::Both(l, _) => {
                if diff {
                    writeln!(output, " {l}").expect("write will never fail");
                } else {
                    writeln!(output, "{l}").expect("write will never fail");
                }
            }
            diff::Result::Right(r) => {
                let trimmed = r.trim();
                if trimmed != "---" && !trimmed.is_empty() {
                    if diff {
                        writeln!(output, "+{r}").expect("write will never fail");
                    } else {
                        writeln!(output, "{r}").expect("write will never fail");
                    }
                }
            }
        }
    }
    Ok(output)
}

fn cleanup(value: &mut Value) {
    match value {
        Value::Null => {}
        Value::Bool(_) => {}
        Value::Number(_) => {}
        Value::String(_) => {}
        Value::Array(a) => {
            a.retain(|v| &Value::String(REMOVAL_VALUE.to_string()) != v && *v != Value::Null);
            for value in a {
                cleanup(value);
            }
        }
        Value::Object(o) => {
            o.retain(|_, v| &Value::String(REMOVAL_VALUE.to_string()) != v && *v != Value::Null);
            for value in o.values_mut() {
                cleanup(value);
            }
        }
    }
}

#[cfg(test)]
mod test {
    use serde_json::json;
    use serde_json::Value;

    use crate::configuration::upgrade::apply_migration;
    use crate::configuration::upgrade::generate_upgrade_output;
    use crate::configuration::upgrade::Action;
    use crate::configuration::upgrade::Migration;

    fn source_doc() -> Value {
        json!( {
          "obj": {
                "field1": 1,
                "field2": 2
            },
          "arr": [
                "v1",
                "v2"
            ]
        })
    }

    #[test]
    fn delete_field() {
        insta::assert_json_snapshot!(apply_migration(
            &source_doc(),
            &Migration::builder()
                .action(Action::Delete {
                    path: "obj.field1".to_string()
                })
                .description("delete field1")
                .build(),
        )
        .expect("expected successful migration"));
    }

    #[test]
    fn delete_array_element() {
        insta::assert_json_snapshot!(apply_migration(
            &source_doc(),
            &Migration::builder()
                .action(Action::Delete {
                    path: "arr.0".to_string()
                })
                .description("delete arr[0]")
                .build(),
        )
        .expect("expected successful migration"));
    }

    #[test]
    fn move_field() {
        insta::assert_json_snapshot!(apply_migration(
            &source_doc(),
            &Migration::builder()
                .action(Action::Move {
                    from: "obj.field1".to_string(),
                    to: "new.obj.field1".to_string()
                })
                .description("move field1")
                .build(),
        )
        .expect("expected successful migration"));
    }

    #[test]
    fn add_field() {
        // This one won't add the field because `obj.field1` already exists
        insta::assert_json_snapshot!(apply_migration(
            &source_doc(),
            &Migration::builder()
                .action(Action::Add {
                    path: "obj".to_string(),
                    name: "field1".to_string(),
                    value: 25.into()
                })
                .description("add field1")
                .build(),
        )
        .expect("expected successful migration"));

        insta::assert_json_snapshot!(apply_migration(
            &source_doc(),
            &Migration::builder()
                .action(Action::Add {
                    path: "obj".to_string(),
                    name: "field3".to_string(),
                    value: 42.into()
                })
                .description("add field3")
                .build(),
        )
        .expect("expected successful migration"));

        // This one won't add the field because `unexistent` doesn't exist, we don't add parent structure
        insta::assert_json_snapshot!(apply_migration(
            &source_doc(),
            &Migration::builder()
                .action(Action::Add {
                    path: "unexistent".to_string(),
                    name: "field".to_string(),
                    value: 1.into()
                })
                .description("add field3")
                .build(),
        )
        .expect("expected successful migration"));
    }

    #[test]
    fn move_non_existent_field() {
        insta::assert_json_snapshot!(apply_migration(
            &json!({"should": "stay"}),
            &Migration::builder()
                .action(Action::Move {
                    from: "obj.field1".to_string(),
                    to: "new.obj.field1".to_string()
                })
                .description("move field1")
                .build(),
        )
        .expect("expected successful migration"));
    }

    #[test]
    fn move_array_element() {
        insta::assert_json_snapshot!(apply_migration(
            &source_doc(),
            &Migration::builder()
                .action(Action::Move {
                    from: "arr.0".to_string(),
                    to: "new.arr.0".to_string()
                })
                .description("move arr[0]")
                .build(),
        )
        .expect("expected successful migration"));
    }

    #[test]
    fn copy_field() {
        insta::assert_json_snapshot!(apply_migration(
            &source_doc(),
            &Migration::builder()
                .action(Action::Copy {
                    from: "obj.field1".to_string(),
                    to: "new.obj.field1".to_string()
                })
                .description("copy field1")
                .build(),
        )
        .expect("expected successful migration"));
    }

    #[test]
    fn copy_array_element() {
        insta::assert_json_snapshot!(apply_migration(
            &source_doc(),
            &Migration::builder()
                .action(Action::Copy {
                    from: "arr.0".to_string(),
                    to: "new.arr.0".to_string()
                })
                .description("copy arr[0]")
                .build(),
        )
        .expect("expected successful migration"));
    }

    #[test]
    fn diff_upgrade_output() {
        insta::assert_snapshot!(generate_upgrade_output(
            "changed: bar\nstable: 1.0\ndeleted: gone",
            "changed: bif\nstable: 1.0\nadded: new",
            true
        )
        .expect("expected successful migration"));
    }

    #[test]
    fn upgrade_output() {
        insta::assert_snapshot!(generate_upgrade_output(
            "changed: bar\nstable: 1.0\ndeleted: gone",
            "changed: bif\nstable: 1.0\nadded: new",
            false
        )
        .expect("expected successful migration"));
    }

    #[test]
    fn change_field() {
        insta::assert_json_snapshot!(apply_migration(
            &source_doc(),
            &Migration::builder()
                .action(Action::Change {
                    path: "obj.field1".to_string(),
                    from: Value::Number(1u64.into()),
                    to: Value::String("a".into()),
                })
                .description("change field1")
                .build(),
        )
        .expect("expected successful migration"));
    }
}
