#!/usr/bin/expect -f

set timeout -1
spawn {*}$argv

expect "Author password: "
send -- "$env(CERT_AUTHOR_PASSWORD)\n"
expect "Yes: (Y), No: (N) ?"
send -- "n\n"

expect "Distributor1 password: "
send -- "$env(CERT_DIST_PASSWORD)\n"
expect "Yes: (Y), No: (N) ?"
send -- "n\n"

expect eof
