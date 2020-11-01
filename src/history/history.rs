#![allow(clippy::module_inception)]
use crate::shell_history;
use rusqlite::{Connection, MappedRows, Row, NO_PARAMS};
use std::cmp::Ordering;
use std::io::Write;
use std::path::PathBuf;
use std::{fmt, fs, io};
//use std::time::Instant;
use crate::history::{db_extensions, schema};
use crate::network::Network;
use crate::path_update_helpers;
use crate::settings::{HistoryFormat, Settings};
use crate::simplified_command::SimplifiedCommand;
use rusqlite::types::ToSql;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use itertools::Itertools;

#[derive(Debug, Clone, Default)]
pub struct Features {
    pub age_factor: f64,
    pub length_factor: f64,
    pub exit_factor: f64,
    pub recent_failure_factor: f64,
    pub selected_dir_factor: f64,
    pub dir_factor: f64,
    pub overlap_factor: f64,
    pub immediate_overlap_factor: f64,
    pub selected_occurrences_factor: f64,
    pub occurrences_factor: f64,
}

#[derive(Debug, Clone, Default)]
pub struct Command {
    pub id: i64,
    pub cmd: String,
    pub cmd_tpl: String,
    pub session_id: String,
    pub rank: f64,
    pub when_run: Option<i64>,
    pub exit_code: Option<i32>,
    pub selected: bool,
    pub dir: Option<String>,
    pub features: Features,
    pub match_bounds: Vec<(usize, usize)>
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.cmd.fmt(f)
    }
}

impl From<Command> for String {
    fn from(command: Command) -> Self {
        command.cmd
    }
}

#[derive(Debug)]
pub struct History {
    pub connection: Connection,
    pub network: Network,
}

const IGNORED_COMMANDS: [&str; 7] = [
    "pwd",
    "ls",
    "cd",
    "cd ..",
    "clear",
    "history",
    "mcfly search",
];

impl History {
    pub fn load(history_format: HistoryFormat) -> History {
        let db_path = Settings::mcfly_db_path();
        let history = if db_path.exists() {
            History::from_db_path(db_path)
        } else {
            History::from_shell_history(history_format)
        };
        schema::migrate(&history.connection);
        history
    }

    pub fn should_add(&self, command: &str) -> bool {
        // Ignore empty commands.
        if command.is_empty() {
            return false;
        }

        // Ignore commands added via a ctrl-r search.
        if command.starts_with("#mcfly:") {
            return false;
        }

        // Ignore commands with a leading space.
        if command.starts_with(' ') {
            return false;
        }

        // Ignore blacklisted commands.
        if IGNORED_COMMANDS.contains(&command) {
            return false;
        }

        // Ignore the previous command (independent of Session ID) so that opening a new terminal
        // window won't replay the last command in the history.
        let last_command = self.last_command(&None);
        if last_command.is_none() {
            return true;
        }
        !command.eq(&last_command.unwrap().cmd)
    }

    pub fn add(
        &self,
        command: &str,
        session_id: &str,
        dir: &str,
        when_run: &Option<i64>,
        exit_code: Option<i32>,
        old_dir: &Option<String>,
    ) {
        self.possibly_update_paths(command, exit_code);
        let selected = self.determine_if_selected_from_ui(command, session_id, dir);
        let simplified_command = SimplifiedCommand::new(command, true);
        self.connection.execute_named("INSERT INTO commands (cmd, cmd_tpl, session_id, when_run, exit_code, selected, dir, old_dir) VALUES (:cmd, :cmd_tpl, :session_id, :when_run, :exit_code, :selected, :dir, :old_dir)",
                                      &[
                                          (":cmd", &command.to_owned()),
                                          (":cmd_tpl", &simplified_command.result.to_owned()),
                                          (":session_id", &session_id.to_owned()),
                                          (":when_run", &when_run.to_owned()),
                                          (":exit_code", &exit_code.to_owned()),
                                          (":selected", &selected),
                                          (":dir", &dir.to_owned()),
                                          (":old_dir", &old_dir.to_owned()),
                                      ]).unwrap_or_else(|err| panic!(format!("McFly error: Insert into commands to work ({})", err)));
    }

    fn determine_if_selected_from_ui(&self, command: &str, session_id: &str, dir: &str) -> bool {
        let rows_affected = self
            .connection
            .execute_named(
                "DELETE FROM selected_commands \
                 WHERE cmd = :cmd \
                 AND session_id = :session_id \
                 AND dir = :dir",
                &[
                    (":cmd", &command.to_owned()),
                    (":session_id", &session_id.to_owned()),
                    (":dir", &dir.to_owned()),
                ],
            )
            .unwrap_or_else(|err| {
                panic!(format!(
                    "McFly error: DELETE from selected_commands to work ({})",
                    err
                ))
            });

        // Delete any other pending selected commands for this session -- they must have been aborted or edited.
        self.connection
            .execute_named(
                "DELETE FROM selected_commands WHERE session_id = :session_id",
                &[(":session_id", &session_id.to_owned())],
            )
            .unwrap_or_else(|err| {
                panic!(format!(
                    "McFly error: DELETE from selected_commands to work ({})",
                    err
                ))
            });

        rows_affected > 0
    }

    pub fn record_selected_from_ui(&self, command: &str, session_id: &str, dir: &str) {
        self.connection.execute_named("INSERT INTO selected_commands (cmd, session_id, dir) VALUES (:cmd, :session_id, :dir)",
                                      &[
                                          (":cmd", &command.to_owned()),
                                          (":session_id", &session_id.to_owned()),
                                          (":dir", &dir.to_owned())
                                      ]).unwrap_or_else(|err| panic!(format!("McFly error: Insert into selected_commands to work ({})", err)));
    }

    // Update historical paths in our database if a directory has been renamed or moved.
    pub fn possibly_update_paths(&self, command: &str, exit_code: Option<i32>) {
        let successful = exit_code.is_none() || exit_code.unwrap() == 0;
        let is_move =
            |c: &str| c.to_lowercase().starts_with("mv ") && !c.contains('*') && !c.contains('?');
        if successful && is_move(command) {
            let parts = path_update_helpers::parse_mv_command(command);
            if parts.len() == 2 {
                let normalized_from = path_update_helpers::normalize_path(&parts[0]);
                let normalized_to = path_update_helpers::normalize_path(&parts[1]);

                // If $to/$(base_name($from)) exists, and is a directory, assume we've moved $from into $to.
                // If not, assume we've renamed $from to $to.

                if let Some(basename) = PathBuf::from(&normalized_from).file_name() {
                    if let Some(utf8_basename) = basename.to_str() {
                        if utf8_basename.contains('.') {
                            // It was probably a file.
                            return;
                        }
                        let maybe_moved_directory =
                            PathBuf::from(&normalized_to).join(utf8_basename);
                        if maybe_moved_directory.exists() {
                            if maybe_moved_directory.is_dir() {
                                self.update_paths(
                                    &normalized_from,
                                    maybe_moved_directory.to_str().unwrap(),
                                    false,
                                );
                            } else {
                                // The source must have been a file, so ignore it.
                            }
                            return;
                        }
                    } else {
                        // Don't try to handle non-utf8 filenames, at least for now.
                        return;
                    }
                }

                let to_pathbuf = PathBuf::from(&normalized_to);
                if to_pathbuf.exists() && to_pathbuf.is_dir() {
                    self.update_paths(&normalized_from, &normalized_to, false);
                }
            }
        }
    }

    pub fn find_matches(&self, cmd: &str, num: i16, fuzzy: bool) -> Vec<Command> {
        let mut like_query = "%".to_string();

        if fuzzy {
            like_query.push_str(&cmd.split("").collect::<Vec<&str>>().join("%"));
        } else {
            like_query.push_str(cmd);
        }

        like_query.push_str("%");

        let query = "SELECT id, cmd, cmd_tpl, session_id, when_run, exit_code, selected, dir, rank,
                                  age_factor, length_factor, exit_factor, recent_failure_factor,
                                  selected_dir_factor, dir_factor, overlap_factor, immediate_overlap_factor,
                                  selected_occurrences_factor, occurrences_factor
                           FROM contextual_commands
                           WHERE cmd LIKE (:like)
                           ORDER BY rank DESC LIMIT :limit";
        let mut statement = self
            .connection
            .prepare(query)
            .unwrap_or_else(|err| panic!(format!("McFly error: Prepare to work ({})", err)));
        let command_iter = statement
            .query_map_named(&[(":like", &like_query), (":limit", &num)], |row| {
                let text:String = row.get_checked(1).unwrap_or_else(|err| {
                    panic!(format!("McFly error: cmd to be readable ({})", err))
                });
                let lowercase_text = text.to_lowercase();
                let lowercase_cmd = cmd.to_lowercase();

                let bounds = match fuzzy {
                    true => {
                        let mut search_iter = lowercase_cmd.chars().peekable();
                        let mut matches = lowercase_text.match_indices(|c| {
                            let next = search_iter.peek();

                            if next.is_some() && next.unwrap() == &c {
                                let _advance = search_iter.next();

                                return true;
                            }

                            return false;
                        }).map(|m| m.0);

                        let start = matches.next().unwrap_or(0);
                        let end = matches.last().unwrap_or(start) + 1;

                        vec![(start, end)]
                    },
                    false => lowercase_text
                        .match_indices(&lowercase_cmd)
                        .map(|(index, _)| (index, index + cmd.len()))
                        .collect::<Vec<_>>()
                };

                Command {
                    id: row.get_checked(0).unwrap_or_else(|err| {
                        panic!(format!("McFly error: id to be readable ({})", err))
                    }),
                    cmd: text,
                    cmd_tpl: row.get_checked(2).unwrap_or_else(|err| {
                        panic!(format!("McFly error: cmd_tpl to be readable ({})", err))
                    }),
                    session_id: row.get_checked(3).unwrap_or_else(|err| {
                        panic!(format!("McFly error: session_id to be readable ({})", err))
                    }),
                    when_run: row.get_checked(4).unwrap_or_else(|err| {
                        panic!(format!("McFly error: when_run to be readable ({})", err))
                    }),
                    exit_code: row.get_checked(5).unwrap_or_else(|err| {
                        panic!(format!("McFly error: exit_code to be readable ({})", err))
                    }),
                    selected: row.get_checked(6).unwrap_or_else(|err| {
                        panic!(format!("McFly error: selected to be readable ({})", err))
                    }),
                    dir: row.get_checked(7).unwrap_or_else(|err| {
                        panic!(format!("McFly error: dir to be readable ({})", err))
                    }),
                    rank: row.get_checked(8).unwrap_or_else(|err| {
                        panic!(format!("McFly error: rank to be readable ({})", err))
                    }),
                    match_bounds: bounds,
                    features: Features {
                        age_factor: row.get_checked(9).unwrap_or_else(|err| {
                            panic!(format!("McFly error: age_factor to be readable ({})", err))
                        }),
                        length_factor: row.get_checked(10).unwrap_or_else(|err| {
                            panic!(format!(
                                    "McFly error: length_factor to be readable ({})",
                                    err
                            ))
                        }),
                        exit_factor: row.get_checked(11).unwrap_or_else(|err| {
                            panic!(format!("McFly error: exit_factor to be readable ({})", err))
                        }),
                        recent_failure_factor: row.get_checked(12).unwrap_or_else(|err| {
                            panic!(format!(
                                    "McFly error: recent_failure_factor to be readable ({})",
                                    err
                            ))
                        }),
                        selected_dir_factor: row.get_checked(13).unwrap_or_else(|err| {
                            panic!(format!(
                                    "McFly error: selected_dir_factor to be readable ({})",
                                    err
                            ))
                        }),
                        dir_factor: row.get_checked(14).unwrap_or_else(|err| {
                            panic!(format!("McFly error: dir_factor to be readable ({})", err))
                        }),
                        overlap_factor: row.get_checked(15).unwrap_or_else(|err| {
                            panic!(format!(
                                    "McFly error: overlap_factor to be readable ({})",
                                    err
                            ))
                        }),
                        immediate_overlap_factor: row.get_checked(16).unwrap_or_else(|err| {
                            panic!(format!(
                                    "McFly error: immediate_overlap_factor to be readable ({})",
                                    err
                            ))
                        }),
                        selected_occurrences_factor: row.get_checked(17).unwrap_or_else(|err| {
                            panic!(format!(
                                    "McFly error: selected_occurrences_factor to be readable ({})",
                                    err
                            ))
                        }),
                        occurrences_factor: row.get_checked(18).unwrap_or_else(|err| {
                            panic!(format!(
                                    "McFly error: occurrences_factor to be readable ({})",
                                    err
                            ))
                        }),
                    },
                }
            })
        .unwrap_or_else(|err| panic!(format!("McFly error: Query Map to work ({})", err)));

        let mut names = Vec::new();
        for result in command_iter {
            names.push(result.unwrap_or_else(|err| {
                panic!(format!(
                    "McFly error: Unable to load command from DB ({})",
                    err
                ))
            }));
        }

        if fuzzy {
            names = names.into_iter().sorted_by(|a, b| {
                // results are already sorted by rank, but with fuzzy mode we
                // need to prioritize shorter matches as well, and can only do
                // that at runtime. Each match's rank is weighted by the
                // inverse of its length (relative to both matches) to give
                // short but lower-ranked matches a chance to beat higher-
                // ranked but longer matches.

                let a_len = a.match_bounds[0].1 - a.match_bounds[0].0;
                let b_len = b.match_bounds[0].1 - b.match_bounds[0].0;
                let a_mod = 1.0 - a_len as f64 / (a_len + b_len) as f64;
                let b_mod = 1.0 - b_len as f64 / (a_len + b_len) as f64;

                PartialOrd::partial_cmp(&(b.rank + b_mod), &(a.rank + a_mod)).unwrap_or_else(|| Ordering::Equal)
            }).collect()
        }

        names
    }

    pub fn build_cache_table(
        &self,
        dir: &str,
        session_id: &Option<String>,
        start_time: Option<i64>,
        end_time: Option<i64>,
        now: Option<i64>,
    ) {
        let lookback: u16 = 3;

        let mut last_commands = self.last_command_templates(session_id, lookback as i16, 0);
        if last_commands.len() < lookback as usize {
            last_commands = self.last_command_templates(&None, lookback as i16, 0);
            while last_commands.len() < lookback as usize {
                last_commands.push(String::from(""));
            }
        }

        self.connection
            .execute("DROP TABLE IF EXISTS temp.contextual_commands;", NO_PARAMS)
            .unwrap_or_else(|err| {
                panic!(format!(
                    "McFly error: Removal of temp table to work ({})",
                    err
                ))
            });

        let (mut when_run_min, when_run_max): (f64, f64) = self
            .connection
            .query_row(
                "SELECT MIN(when_run), MAX(when_run) FROM commands",
                NO_PARAMS,
                |row| (row.get(0), row.get(1)),
            )
            .unwrap_or_else(|err| panic!(format!("McFly error: Query to work ({})", err)));

        if (when_run_min - when_run_max).abs() < std::f64::EPSILON {
            when_run_min -= 60.0 * 60.0;
        }

        let max_occurrences: f64 = self
            .connection
            .query_row(
                "SELECT COUNT(*) AS c FROM commands GROUP BY cmd ORDER BY c DESC LIMIT 1",
                NO_PARAMS,
                |row| row.get(0),
            )
            .unwrap_or(1.0);

        let max_selected_occurrences: f64 = self.connection
            .query_row("SELECT COUNT(*) AS c FROM commands WHERE selected = 1 GROUP BY cmd ORDER BY c DESC LIMIT 1", NO_PARAMS,
                       |row| row.get(0)).unwrap_or(1.0);

        let max_length: f64 = self
            .connection
            .query_row("SELECT MAX(LENGTH(cmd)) FROM commands", NO_PARAMS, |row| {
                row.get(0)
            })
            .unwrap_or(100.0);

        #[allow(unused_variables)]
        let beginning_of_execution = Instant::now();
        self.connection.execute_named(
            "CREATE TEMP TABLE contextual_commands AS SELECT
                  id, cmd, cmd_tpl, session_id, when_run, exit_code, selected, dir,

                  /* to be filled in later */
                  0.0 AS rank,

                  /* length of the command string */
                  LENGTH(c.cmd) / :max_length AS length_factor,

                  /* age of the last execution of this command (0.0 is new, 1.0 is old) */
                  MIN((:when_run_max - when_run) / :history_duration) AS age_factor,

                  /* average error state (1: always successful, 0: always errors) */
                  SUM(CASE WHEN exit_code = 0 THEN 1.0 ELSE 0.0 END) / COUNT(*) as exit_factor,

                  /* recent failure (1 if failed recently, 0 if not) */
                  MAX(CASE WHEN exit_code != 0 AND :now - when_run < 120 THEN 1.0 ELSE 0.0 END) AS recent_failure_factor,

                  /* percentage run in this directory (1: always run in this directory, 0: never run in this directory) */
                  SUM(CASE WHEN dir = :directory THEN 1.0 ELSE 0.0 END) / COUNT(*) as dir_factor,

                  /* percentage of time selected in this directory (1: only selected in this dir, 0: only selected elsewhere) */
                  SUM(CASE WHEN dir = :directory AND selected = 1 THEN 1.0 ELSE 0.0 END) / (SUM(CASE WHEN selected = 1 THEN 1.0 ELSE 0.0 END) + 1) as selected_dir_factor,

                  /* average contextual overlap of this command (0: none of the last 3 commands has ever overlapped with this command, 1: all of the last three commands always overlap with this command) */
                  SUM((
                    SELECT COUNT(DISTINCT c2.cmd_tpl) FROM commands c2
                    WHERE c2.id >= c.id - :lookback AND c2.id < c.id AND c2.cmd_tpl IN (:last_commands0, :last_commands1, :last_commands2)
                  ) / :lookback_f64) / COUNT(*) AS overlap_factor,

                  /* average overlap with the last command (0: this command never follows the last command, 1: this command always follows the last command) */
                  SUM((SELECT COUNT(*) FROM commands c2 WHERE c2.id = c.id - 1 AND c2.cmd_tpl = :last_commands0)) / COUNT(*) AS immediate_overlap_factor,

                  /* percentage selected (1: this is the most commonly selected command, 0: this command is never selected) */
                  SUM(CASE WHEN selected = 1 THEN 1.0 ELSE 0.0 END) / :max_selected_occurrences AS selected_occurrences_factor,

                  /* percentage of time this command is run relative to the most common command (1: this is the most common command, 0: this is the least common command) */
                  COUNT(*) / :max_occurrences AS occurrences_factor

                  FROM commands c WHERE when_run > :start_time AND when_run < :end_time GROUP BY cmd ORDER BY id DESC;",
            &[
                (":when_run_max", &when_run_max),
                (":history_duration", &(when_run_max - when_run_min)),
                (":directory", &dir.to_owned()),
                (":max_occurrences", &max_occurrences),
                (":max_length", &max_length),
                (":max_selected_occurrences", &max_selected_occurrences),
                (":lookback", &lookback),
                (":lookback_f64", &(lookback as f64)),
                (":last_commands0", &last_commands[0].to_owned()),
                (":last_commands1", &last_commands[1].to_owned()),
                (":last_commands2", &last_commands[2].to_owned()),
                (":start_time", &start_time.unwrap_or(0).to_owned()),
                (":end_time", &end_time.unwrap_or(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_else(|err| panic!(format!("McFly error: Time went backwards ({})", err))).as_secs() as i64).to_owned()),
                (":now", &now.unwrap_or(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_else(|err| panic!(format!("McFly error: Time went backwards ({})", err))).as_secs() as i64).to_owned())
            ]).unwrap_or_else(|err| panic!(format!("McFly error: Creation of temp table to work ({})", err)));

        self.connection
            .execute(
                "UPDATE contextual_commands
                 SET rank = nn_rank(age_factor, length_factor, exit_factor,
                                    recent_failure_factor, selected_dir_factor, dir_factor,
                                    overlap_factor, immediate_overlap_factor,
                                    selected_occurrences_factor, occurrences_factor);",
                NO_PARAMS,
            )
            .unwrap_or_else(|err| {
                panic!(format!(
                    "McFly error: Ranking of temp table to work ({})",
                    err
                ))
            });

        self.connection
            .execute(
                "CREATE INDEX temp.MyIndex ON contextual_commands(id);",
                NO_PARAMS,
            )
            .unwrap_or_else(|err| {
                panic!(format!(
                    "McFly error: Creation of index on temp table to work ({})",
                    err
                ))
            });

        // println!("Seconds: {}", (beginning_of_execution.elapsed().as_secs() as f64) + (beginning_of_execution.elapsed().subsec_nanos() as f64 / 1000_000_000.0));
    }

    pub fn commands(
        &self,
        session_id: &Option<String>,
        num: i16,
        offset: u16,
        random: bool,
    ) -> Vec<Command> {
        let order = if random { "RANDOM()" } else { "id" };
        let query = if session_id.is_none() {
            format!("SELECT id, cmd, cmd_tpl, session_id, when_run, exit_code, selected, dir FROM commands ORDER BY {} DESC LIMIT :limit OFFSET :offset", order)
        } else {
            format!("SELECT id, cmd, cmd_tpl, session_id, when_run, exit_code, selected, dir FROM commands WHERE session_id = :session_id ORDER BY {} DESC LIMIT :limit OFFSET :offset", order)
        };

        if session_id.is_none() {
            self.run_query(&query, &[(":limit", &num), (":offset", &offset)])
        } else {
            self.run_query(
                &query,
                &[
                    (":session_id", &session_id.to_owned().unwrap()),
                    (":limit", &num),
                    (":offset", &offset),
                ],
            )
        }
    }

    fn run_query(&self, query: &str, params: &[(&str, &dyn ToSql)]) -> Vec<Command> {
        let mut statement = self.connection.prepare(query).unwrap();

        let closure: fn(&Row) -> Command = |row| Command {
            id: row.get(0),
            cmd: row.get(1),
            cmd_tpl: row.get(2),
            session_id: row.get(3),
            when_run: row.get(4),
            exit_code: row.get(5),
            selected: row.get(6),
            dir: row.get(7),
            ..Command::default()
        };

        let command_iter: MappedRows<_> = statement
            .query_map_named(params, closure)
            .unwrap_or_else(|err| panic!(format!("McFly error: Query Map to work ({})", err)));

        let mut vec = Vec::new();
        for result in command_iter {
            if let Ok(command) = result {
                vec.push(command);
            }
        }

        vec
    }

    pub fn last_command(&self, session_id: &Option<String>) -> Option<Command> {
        self.commands(session_id, 1, 0, false).get(0).cloned()
    }

    pub fn last_command_templates(
        &self,
        session_id: &Option<String>,
        num: i16,
        offset: u16,
    ) -> Vec<String> {
        self.commands(session_id, num, offset, false)
            .iter()
            .map(|command| command.cmd_tpl.to_owned())
            .collect()
    }

    pub fn delete_command(&self, command: &str) {
        self.connection
            .execute_named(
                "DELETE FROM selected_commands WHERE cmd = :command",
                &[(":command", &command)],
            )
            .unwrap_or_else(|err| {
                panic!(format!(
                    "McFly error: DELETE from selected_commands to work ({})",
                    err
                ))
            });

        self.connection
            .execute_named(
                "DELETE FROM commands WHERE cmd = :command",
                &[(":command", &command)],
            )
            .unwrap_or_else(|err| {
                panic!(format!(
                    "McFly error: DELETE from commands to work ({})",
                    err
                ))
            });
    }

    pub fn update_paths(&self, old_path: &str, new_path: &str, print_output: bool) {
        let normalized_old_path = path_update_helpers::normalize_path(old_path);
        let normalized_new_path = path_update_helpers::normalize_path(new_path);

        if normalized_old_path.len() > 1 && normalized_new_path.len() > 1 {
            let like_query = normalized_old_path.to_string() + "/%";

            let mut dir_update_statement = self.connection.prepare(
                "UPDATE commands SET dir = :new_dir || SUBSTR(dir, :length) WHERE dir = :exact OR dir LIKE (:like)"
            ).unwrap();

            let mut old_dir_update_statement = self.connection.prepare(
                "UPDATE commands SET old_dir = :new_dir || SUBSTR(old_dir, :length) WHERE old_dir = :exact OR old_dir LIKE (:like)"
            ).unwrap();

            let affected = dir_update_statement
                .execute_named(&[
                    (":like", &like_query),
                    (":exact", &normalized_old_path),
                    (":new_dir", &normalized_new_path),
                    (":length", &(normalized_old_path.chars().count() as u32 + 1)),
                ])
                .unwrap_or_else(|err| panic!(format!("McFly error: dir UPDATE to work ({})", err)));

            old_dir_update_statement
                .execute_named(&[
                    (":like", &like_query),
                    (":exact", &normalized_old_path),
                    (":new_dir", &normalized_new_path),
                    (":length", &(normalized_old_path.chars().count() as u32 + 1)),
                ])
                .unwrap_or_else(|err| {
                    panic!(format!("McFly error: old_dir UPDATE to work ({})", err))
                });

            if print_output {
                println!(
                    "McFly: Command database paths renamed from {} to {} (affected {} commands)",
                    normalized_old_path, normalized_new_path, affected
                );
            }
        } else if print_output {
            println!("McFly: Not updating paths due to invalid options.");
        }
    }

    fn from_shell_history(history_format: HistoryFormat) -> History {
        print!(
            "McFly: Importing shell history for the first time. This may take a minute or two..."
        );
        io::stdout().flush().unwrap_or_else(|err| {
            panic!(format!("McFly error: STDOUT flush should work ({})", err))
        });

        // Load this first to make sure it works before we create the DB.
        let commands =
            shell_history::full_history(&shell_history::history_file_path(), history_format);

        // Make ~/.mcfly
        fs::create_dir_all(Settings::storage_dir_path())
            .unwrap_or_else(|_| panic!("Unable to create {:?}", Settings::storage_dir_path()));

        // Make ~/.mcfly/history.db
        let connection = Connection::open(Settings::mcfly_db_path()).unwrap_or_else(|_| {
            panic!(
                "Unable to create history DB at {:?}",
                Settings::mcfly_db_path()
            )
        });
        db_extensions::add_db_functions(&connection);

        connection.execute_batch(
            "CREATE TABLE commands( \
                      id INTEGER PRIMARY KEY AUTOINCREMENT, \
                      cmd TEXT NOT NULL, \
                      cmd_tpl TEXT, \
                      session_id TEXT NOT NULL, \
                      when_run INTEGER NOT NULL, \
                      exit_code INTEGER NOT NULL, \
                      selected INTEGER NOT NULL, \
                      dir TEXT, \
                      old_dir TEXT \
                  ); \
                  CREATE INDEX command_cmds ON commands (cmd);\
                  CREATE INDEX command_session_id ON commands (session_id);\
                  CREATE INDEX command_dirs ON commands (dir);\
                  \
                  CREATE TABLE selected_commands( \
                      id INTEGER PRIMARY KEY AUTOINCREMENT, \
                      cmd TEXT NOT NULL, \
                      session_id TEXT NOT NULL, \
                      dir TEXT NOT NULL \
                  ); \
                  CREATE INDEX selected_command_session_cmds ON selected_commands (session_id, cmd);"
        ).unwrap_or_else(|err| panic!(format!("McFly error: Unable to initialize history db ({})", err)));

        {
            let mut statement = connection
                .prepare("INSERT INTO commands (cmd, cmd_tpl, session_id, when_run, exit_code, selected) VALUES (:cmd, :cmd_tpl, :session_id, :when_run, :exit_code, :selected)")
                .unwrap_or_else(|err| panic!(format!("McFly error: Unable to prepare insert ({})", err)));
            for command in commands {
                if !IGNORED_COMMANDS.contains(&command.command.as_str()) {
                    let simplified_command = SimplifiedCommand::new(&command.command, true);
                    if !command.command.is_empty() && !simplified_command.result.is_empty() {
                        statement
                            .execute_named(&[
                                (":cmd", &command.command),
                                (":cmd_tpl", &simplified_command.result.to_owned()),
                                (":session_id", &"IMPORTED"),
                                (":when_run", &command.when),
                                (":exit_code", &0),
                                (":selected", &0),
                            ])
                            .unwrap_or_else(|err| {
                                panic!(format!("McFly error: Insert to work ({})", err))
                            });
                    }
                }
            }
        }

        schema::first_time_setup(&connection);

        println!("done.");

        History {
            connection,
            network: Network::default(),
        }
    }

    fn from_db_path(path: PathBuf) -> History {
        let connection = Connection::open(path).unwrap_or_else(|err| {
            panic!(format!(
                "McFly error: Unable to open history database ({})",
                err
            ))
        });
        db_extensions::add_db_functions(&connection);
        History {
            connection,
            network: Network::default(),
        }
    }
}
