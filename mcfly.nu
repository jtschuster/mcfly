# function to call mcfly like: mcfly --history_format $MCFLY_HISTORY_FORMAT search -o "${mcfly_output}" "${LBUFFER}"

def invoke_mcfly [partialCommand] {
    D:/mcfly/target/debug/mcfly.exe search -o output.txt $partialCommand
    let command = open output.txt | lines | each { $in | split words | skip 1 | str join ' ' } | select 1
    $env.config.keybindings = ($env.config.keybindings | each { if $in.name == "mcfly_insert" { $in | update event { edit: insertstring, value: $command } } else { $in } })
}


$env.config.keybindings = ($env.config.keybindings | each { if $in.name == "history_menu" { $in | update event [{ send : executehostcommand, cmd: "invoke_mcfly idk" }, { send:  } ]} else { $in } })

$env.config.keybindings = ($env.config.keybindings | each { if $in.name == "mcfly_insert" { $in | update event { edit: insertstring, value: $command } } else { $in } })

$env.MCFLY_HISTORY = $nu.history-path
$env.MCFLY_SESSION_ID = "asdfasfdasfd"