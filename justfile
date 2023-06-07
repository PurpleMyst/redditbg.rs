roaming_appdata := env_var("APPDATA")
local_appdata := env_var("LOCALAPPDATA")

subpath := "PurpleMyst/redditbg"

[windows]
open-roaming-appdata:
    start '{{roaming_appdata}}/{{subpath}}'

[windows]
open-local-appdata:
    start '{{local_appdata}}/{{subpath}}'

alias ora := open-roaming-appdata
alias ola := open-local-appdata
