## Broadcast 

Broadcast commands to other machines!

## Motivation

I wrote broadcast with the intent to learn more about terminal emulation, what the heck PTY stands for and to solve some personal issues I'd faced while using `WSL <exec>`.

Broadcast can be called like this: "broadcast valgrind" where valgrind is the command that should be "broadcasted" (executed) inside of the machine `broadcast_server` is running on. 

## TODO 

- Let user install aliases (broadcast r / register -c "ls -la" -a ll) so that "ll" will run "ls -la" in WSL
  For that we need to figure out what shell the user is running, powershell, nushell, cmd and need to figure out how to install aliases in these shells
- TODO configuration file
- TODO write a proper readme / blog post
- TODO Give the user an option to address specific distribution with command trough broadcast -d <distro> when using wsl mode
- TODO allow specifying the port
