#!/usr/bin/expect -f

set timeout -1
spawn {*}$argv

expect "SSH Private Key: /root/.ssh/tv_webos"
send -- "$env(PASSPHRASE)\n"

expect eof
