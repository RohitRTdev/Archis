# Userspace

Most of this userspace was written with Claude.

Contains `libc` (see `libc/README.md`), the shell (`sh/`), and a set of small coreutils-style
programs. All binaries link against `libc` and `crt` for startup.

## Shell — `sh/`

A small POSIX-ish shell: parsing (`parse.c`), execution (`exec.c`), job control (`job.h`),
redirection (`redir.c`), and builtins (`builtins.c`).

**Builtins:** `cd`, `pwd`, `export`, `exit`, `source`

Supports pipelines, external command execution, and script execution (`sh script.sh`).

## Coreutils

| Program | Description |
|---|---|
| `cat.c` | Concatenate/print files. Flags: `-n` number lines, `-b` number non-blank lines, `-s` squeeze blank lines, `-E`/`-A` show line ends |
| `cp.c` | Copy files/directories. Flags: `-r`/`-R` recursive, `-f` force, `-i` interactive, `-n` no-clobber, `-v` verbose |
| `mv.c` | Move/rename files. Flags: `-f` force, `-i` interactive, `-n` no-clobber, `-v` verbose |
| `rm.c` | Remove files |
| `rmdir.c` | Remove empty directories |
| `mkdir.c` | Create directories |
| `ln.c` | Create links |
| `readlink.c` | Print resolved symlink/path target |
| `ls.c` | List directory contents. Flags: `-a` show all, `-l` long format |
| `head.c` | Print the first N lines/bytes of a file (supports `-N` to exclude the last N) |
| `tail.c` | Print the last N lines/bytes of a file (supports `+N` to start at absolute offset) |
| `wc.c` | Count lines/words/bytes |
| `echo.c` | Print arguments |
| `dmesg.c` | Print kernel log buffer |
| `ps.c` | List processes |
| `kill.c` | Send a signal to a process |
| `sleep.c` | Sleep for a given duration (accepts NUMBER[SUFFIX], e.g. `s`/`ms`) |
| `shutdown.c` | Shut down the system |
| `init.c` | PID 1 / init process, runs startup scripts |

## Test / example programs

`producer.c`, `producer2.c`, `producer3.c`, `consumer.c` — pipe and IPC exercises.
`signal_test.c`, `suspend_test.c`, `stdio_test.c`, `envtest.c`, `fs_stress.c` — targeted tests
for signals, process suspend, stdio, environment variables, and filesystem stress.
