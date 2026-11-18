#!/usr/bin/env python3
"""End-to-end test of the sshd module (docs/sshd.md), in QEMU, with OpenSSH.

aarch64 (HVF on an Apple Silicon Mac unless --tcg): boots nos-arm64.img from
an ext4 root, loads sshd.ko over the UDP shell, and has the ssh client work
it -- commands, a shell with a terminal and one without, 3 MiB of output
through the client's window with rekeys in the middle, a key it has to
refuse, sessions at once, a stop and an rmmod from inside a session; files
copied in and out over SFTP by OpenSSH's sftp and scp, and the copies' every
refusal -- and
loads edit.ko and works the editor through `ssh -t` on a pty of the test's
own: the terminal's size reaching the command, a resize drawn again whole, a
save, a write over a file changed meanwhile asked about, and changes kept in
<file>.save when the connection goes. Then it
puts the module in /etc/rc, reboots, and checks it came back by itself with
the same host key, and that poweroff unloads it before the filesystems go and
then switches the machine off with nothing panicking on the way.

x86_64: boots nos.iso from an ext4 root that already carries sshd.ko, an
authorized key and an /etc/rc that starts it, logs in, and copies a file in
and back out over SFTP: no shell is touched at all. `--nic` puts QEMU's
82574L (e1000e) or 82576 (igb) where virtio-net is, which runs the session
over that driver.

    tests/sshd-test.py [--arch aarch64|x86_64] [--tcg] [--keep]
                       [--nic virtio|e1000e|igb] [--ssh-port N]

Needs ssh, ssh-keygen, sftp and scp (OpenSSH 8.7 or later, for `scp -s`),
scripts/mkrootfs.sh (mke2fs, or the nos-builder image), host ports 2222
(or --ssh-port's), 8000 and 9000 free, and `make` or `make ARCH=aarch64`
done first.
"""

import argparse
import fcntl
import functools
import hashlib
import http.server
import os
import platform
import pty
import re
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, HERE)

import parttest as pt  # noqa: E402  (tests/parttest.py)

SSH_PORT = 2222
HTTP_PORT = 8000
SHELL_PORT = 9000

# The UDP shell's framing (scripts/udpsh.py)
SHELL_MAGIC = 0x4E4F5348
SHELL_HDR = struct.Struct("!IIHHHH")
SHELL_LAST = 1

BIG = 3 * 1024 * 1024
# What sftp copies in and out: several of the server's messages' worth, and
# no whole number of any of them
SFTP_BIG = 8 * 1024 * 1024 + 12345
# nos's uptime prints seconds: 205.613949
NUMBER = re.compile(r"\b\d+\.\d+\b")

failures = []


def check(name, ok, detail=""):
    print(("PASS: " if ok else "FAIL: ") + name + (" -- " + str(detail) if detail and not ok else ""), flush=True)
    if not ok:
        failures.append(name)
    return ok


def uptime_like(out):
    return bool(NUMBER.search(out))


class Shell:
    """The UDP shell, answering whole."""

    def __init__(self, timeout=90):
        self.seq = 0
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.settimeout(timeout)

    def run(self, cmd):
        self.seq += 1
        body = cmd.encode()
        self.sock.sendto(SHELL_HDR.pack(SHELL_MAGIC, self.seq, 0, 0, len(body), 0) + body, ("127.0.0.1", SHELL_PORT))
        chunks, last = {}, None
        while True:
            data, _ = self.sock.recvfrom(4096)
            magic, seq, idx, flags, length, _ = SHELL_HDR.unpack(data[:SHELL_HDR.size])
            if magic != SHELL_MAGIC or seq != self.seq:
                continue
            chunks[idx] = data[SHELL_HDR.size:SHELL_HDR.size + length]
            if flags & SHELL_LAST:
                last = idx
            if last is not None and len(chunks) == last + 1:
                out = b"".join(chunks[i] for i in range(last + 1)).decode(errors="replace")
                print("  $ " + cmd + "".join("\n    " + line for line in out.splitlines()), flush=True)
                return out


class Test:
    def __init__(self, tmp, tcg):
        self.tmp = tmp
        self.tcg = tcg
        self.key = os.path.join(tmp, "id_test")
        self.other = os.path.join(tmp, "id_other")
        for path in (self.key, self.other):
            subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "sshd-test", "-f", path], check=True)
        self.pub = open(self.key + ".pub").read().strip()

    def ssh(self, args, stdin=None, tty=False, key=None, timeout=180, known_hosts="/dev/null", strict="no"):
        cmd = ["ssh", "-p", str(SSH_PORT), "-i", key or self.key, "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes",
               "-o", "StrictHostKeyChecking=" + strict, "-o", "UserKnownHostsFile=" + known_hosts,
               "-o", "ConnectTimeout=30", "-o", "LogLevel=ERROR"]
        if tty:
            cmd.append("-tt")
        cmd += ["root@127.0.0.1"] + args
        start = time.time()
        try:
            p = subprocess.run(cmd, input=stdin, capture_output=True, timeout=timeout)
        except subprocess.TimeoutExpired:
            print("  ssh %s: timed out" % " ".join(args), flush=True)
            return None, "", "timeout"
        out = p.stdout.decode(errors="replace")
        err = p.stderr.decode(errors="replace")
        print("  ssh %s -> %d in %.1f s, %d bytes" % (" ".join(args) or "(shell)", p.returncode,
                                                      time.time() - start, len(p.stdout)), flush=True)
        return p.returncode, out, err

    def client_options(self):
        return ["-i", self.key, "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null", "-o", "ConnectTimeout=30", "-o", "LogLevel=ERROR"]

    def sftp_batch(self, commands, timeout=600):
        """OpenSSH's sftp, running `commands` (sftp -b): its exit code, what it
        printed and what it said went wrong. A command with `-` in front may
        fail without ending the batch."""
        batch = os.path.join(self.tmp, "batch")
        with open(batch, "w") as f:
            f.write("\n".join(commands) + "\n")
        start = time.time()
        try:
            p = subprocess.run(["sftp", "-P", str(SSH_PORT)] + self.client_options() + ["-b", batch, "root@127.0.0.1"],
                               capture_output=True, timeout=timeout)
        except subprocess.TimeoutExpired:
            print("  sftp %s: timed out" % "; ".join(commands), flush=True)
            return None, "", "timeout"
        out, err = p.stdout.decode(errors="replace"), p.stderr.decode(errors="replace")
        print("  sftp %s -> %d in %.1f s" % ("; ".join(commands), p.returncode, time.time() - start), flush=True)
        return p.returncode, out, err

    def scp(self, args, timeout=600):
        """OpenSSH's scp over SFTP (-s: the default since OpenSSH 9.0, asked
        for by name for the 8.7 to 8.9 that have it and still default to
        the old protocol): its exit code and what it said went wrong."""
        start = time.time()
        p = subprocess.run(["scp", "-s", "-q", "-P", str(SSH_PORT)] + self.client_options() + args, capture_output=True,
                           timeout=timeout)
        err = p.stderr.decode(errors="replace")
        print("  scp %s -> %d in %.1f s" % (" ".join(args), p.returncode, time.time() - start), flush=True)
        return p.returncode, err

    def sftp(self, sh):
        """Files copied in and out over SFTP (docs/sshd.md, "Copying files"):
        OpenSSH's sftp and scp, a file of several MiB both ways, a tree, a
        directory listed, renamed, removed -- and what nos cannot do refused
        by name: permissions, a name longer than the filesystem keeps, a
        directory removed with something in it."""
        big = os.path.join(self.tmp, "sftp-big.bin")
        data = os.urandom(SFTP_BIG)
        with open(big, "wb") as f:
            f.write(data)
        digest = hashlib.sha256(data).hexdigest()
        back = os.path.join(self.tmp, "sftp-back.bin")

        rc, out, err = self.sftp_batch(["mkdir /sftp", "cd /sftp", "put %s big.bin" % big, "ls -l", "df",
                                        "get big.bin %s" % back, "rename big.bin moved.bin", "ls"])
        got = hashlib.sha256(open(back, "rb").read()).hexdigest() if os.path.exists(back) else None
        check("sftp: a file of %d bytes in and back out, the same bytes" % SFTP_BIG, rc == 0 and got == digest,
              (err + out)[-600:])
        check("sftp: ls -l shows it, at its size", re.search(r"-rw-r--r--.*\b%d\b.*big\.bin" % SFTP_BIG, out) is not None,
              out[-600:])
        check("sftp: df", re.search(r"Size\s+Used\s+Avail", out) is not None, out[-600:])
        check("sftp: renamed", "moved.bin" in out.split("rename")[-1], out[-300:])
        rc, out, _ = self.ssh(["ls", "/sftp"])
        check("the kernel's shell sees it, at its size",
              rc == 0 and re.search(r"^f\s+%d\s+moved\.bin$" % SFTP_BIG, out, re.M) is not None, out)
        out = sh.run("dmesg 20 sftp")
        check("the session's line in the log", re.search(r"sshd: root from .*: sftp, \d+ files opened", out) is not None,
              out)

        # scp, which speaks SFTP: in, and back out
        rc, err = self.scp([big, "root@127.0.0.1:/sftp/scp.bin"])
        check("scp in", rc == 0, err)
        os.remove(back)
        rc, err = self.scp(["root@127.0.0.1:/sftp/scp.bin", back])
        got = hashlib.sha256(open(back, "rb").read()).hexdigest() if os.path.exists(back) else None
        check("scp back out, the same bytes", rc == 0 and got == digest, err)

        # A tree, both ways: scp -r makes the directories and copies each file
        tree = os.path.join(self.tmp, "tree")
        os.makedirs(os.path.join(tree, "sub", "deeper"))
        files = {"one.txt": b"one\n", os.path.join("sub", "two.bin"): os.urandom(70000),
                 os.path.join("sub", "deeper", "three"): b"", os.path.join("sub", "a.new"): b"beside a\n",
                 os.path.join("sub", "a"): b"a\n"}
        for name, content in files.items():
            with open(os.path.join(tree, name), "wb") as f:
                f.write(content)
        rc, err = self.scp(["-r", tree, "root@127.0.0.1:/sftp/"])
        check("scp -r in", rc == 0, err)
        copy = os.path.join(self.tmp, "tree-back")
        rc, err = self.scp(["-r", "root@127.0.0.1:/sftp/tree", copy])
        same = rc == 0 and all(os.path.exists(os.path.join(copy, n)) and open(os.path.join(copy, n), "rb").read() == c
                               for n, c in files.items())
        check("scp -r back out, every file the same", same, err)

        # A remove takes the file named and nothing else: a.new beside it
        # stays, as it is a file of its own (the configuration files'
        # <path>.new is theirs)
        rc, out, err = self.sftp_batch(["rm /sftp/tree/sub/a", "ls /sftp/tree/sub"])
        check("sftp: rm takes the file, and a.new beside it stays", rc == 0 and "a.new" in out.split("ls")[-1] and
              not re.search(r"(^|\s)a\s*$", out.split("ls")[-1], re.M), (err + out)[-300:])

        # Refusals, each said
        rc, out, err = self.sftp_batch(["rm /sftp/nothing-here"])
        check("sftp: rm of nothing is refused, no such file", rc != 0 and "No such file" in err, err)
        rc, out, err = self.sftp_batch(["rmdir /sftp/tree"])
        check("sftp: rmdir of a directory with something in it is refused", rc != 0 and "Failure" in err, err)
        rc, out, err = self.sftp_batch(["mkdir /sftp"])
        check("sftp: mkdir where there is one already is refused", rc != 0 and "Failure" in err, err)
        rc, out, err = self.sftp_batch(["chmod 600 /sftp/moved.bin"])
        check("sftp: chmod is refused, nos keeping no permissions", rc != 0 and "unsupported" in err.lower(), err)
        rc, out, err = self.sftp_batch(["put %s /sftp/%s" % (big, "n" * 64)])
        check("sftp: a name of 64 bytes is refused", rc != 0, err)
        rc, out, _ = self.ssh(["ls", "/sftp"])
        check("and nothing of it is left behind", rc == 0 and "n" * 60 not in out, out)
        # scp says so and goes on: OpenSSH 9.9's ends with 0, the data copied
        rc, err = self.scp(["-p", big, "root@127.0.0.1:/sftp/kept.bin"])
        check("scp -p: the times and the mode it would keep are refused, and scp says so",
              "fsetstat" in err and "Operation unsupported" in err, err)

        # Everything taken away again, the tree a file and a directory at a
        # time as sftp's rm and rmdir do
        rc, out, err = self.sftp_batch(["rm /sftp/tree/sub/deeper/three", "rmdir /sftp/tree/sub/deeper",
                                        "rm /sftp/tree/sub/*", "rmdir /sftp/tree/sub", "rm /sftp/tree/one.txt",
                                        "rmdir /sftp/tree", "rm /sftp/*", "rmdir /sftp"])
        rc2, out2, _ = self.ssh(["ls", "/"])
        check("sftp: rm and rmdir take it all away", rc == 0 and rc2 == 0 and re.search(r"\bsftp\b", out2) is None,
              (err + out)[-400:] + out2)

    def ssh_background(self, args, tty=False):
        cmd = ["ssh", "-p", str(SSH_PORT), "-i", self.key, "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes",
               "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR"]
        if tty:
            cmd.append("-tt")
        return subprocess.Popen(cmd + ["root@127.0.0.1"] + args, stdin=subprocess.PIPE,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def wait_ssh(self, timeout):
        start = time.time()
        while time.time() - start < timeout:
            rc, out, _ = self.ssh(["uptime"])
            if rc == 0 and uptime_like(out):
                return True
            time.sleep(3)
        return False

    def mkrootfs(self, image, directory):
        subprocess.run([os.path.join(ROOT, "scripts", "mkrootfs.sh"), image, "64", directory], check=True,
                       stdout=subprocess.DEVNULL)

    def battery(self, sh):
        """What a login server has to get right."""
        rc, out, _ = self.ssh(["uptime"])
        check("a command", rc == 0 and uptime_like(out), out)
        rc, out, _ = self.ssh(["help"])
        check("a longer command, the module's own command in it", rc == 0 and "sshd" in out, out[-300:])
        rc, out, _ = self.ssh(["nosuchcommand"])
        check("a command the kernel does not have", rc == 0 and "not found" in out, out)

        rc, out, _ = self.ssh([], stdin=b"uptime\rversion\rexit\r", tty=True)
        check("a shell with a terminal: prompt, commands, exit", rc == 0 and uptime_like(out) and "$ " in out,
              repr(out[-300:]))
        check("line ends CR LF on a terminal", "\r\n" in out, repr(out[:200]))
        # The line editor's history (shell-commands.md#line-editing): Up
        # twice runs uptime again, ^R and "vers" version
        rc, out, _ = self.ssh([], stdin=b"uptime\rversion\r\x1b[A\x1b[A\r\x12vers\rexit\r", tty=True)
        check("Up and ^R through a terminal's history",
              rc == 0 and len(re.findall(r"^\d+\.\d+\r?$", out, re.M)) == 2 and
              len(re.findall(r"nos \S+ \(", out)) == 2 and "(reverse-i-search)'vers': version" in out,
              repr(out[-600:]))
        rc, out, _ = self.ssh([], stdin=b"uptime\nversion\n")
        check("a shell without one: no prompt, EOF ends it", rc == 0 and uptime_like(out) and "$ " not in out,
              repr(out[-300:]))

        big = open(os.path.join(self.tmp, "big.txt")).read()
        rc, out, _ = self.ssh(["cat", "/big.txt"], timeout=900)
        check("3 MiB through a 2 MiB window", rc == 0 and out.rstrip("\n") == big.rstrip("\n"), "%d bytes" % len(out))
        # The client asks every 16 KiB; a server streaming answers when it next
        # reads -- once the window runs out -- so rekeys land mid-output
        rc, out, err = self.ssh(["-v", "-o", "LogLevel=DEBUG1", "-o", "RekeyLimit=16K", "cat", "/big.txt"], timeout=900)
        kexes = err.count("SSH2_MSG_KEXINIT sent")
        check("rekeys in the middle of the output", rc == 0 and out.rstrip("\n") == big.rstrip("\n") and kexes >= 2,
              "%d bytes, %d key exchanges" % (len(out), kexes))

        # A command that prints nothing for seconds, as `hv wait` does for a
        # minute: the client asks every second whether the server is there
        # and hangs up after two unanswered, as ServerAliveInterval has it.
        # The session tends the connection while the command runs beside it
        rc, out, err = self.ssh(["-o", "ServerAliveInterval=1", "-o", "ServerAliveCountMax=2", "top", "5000"],
                                timeout=120)
        check("keepalives are answered while a quiet command runs", rc == 0 and "5000 ms window" in out,
              (err + out)[-300:])

        rc, _, err = self.ssh(["uptime"], key=self.other)
        check("a key not allowed is refused", rc == 255 and "Permission denied" in err, err)

        procs = [self.ssh_background(["uptime"]) for _ in range(4)]
        outs = [p.communicate(timeout=300)[0].decode(errors="replace") for p in procs]
        check("four sessions at once", all(p.returncode == 0 for p in procs) and all(uptime_like(o) for o in outs),
              [p.returncode for p in procs])

        out = sh.run("sshd")
        check("sshd shows what it serves", "listening" in out and "logins" in out, out)

        # Commands that print holding a spinlock with interrupts off: their
        # output waits in memory and goes once they have returned
        rc, out, _ = self.ssh(["ps"])
        check("ps, printed under the task list's lock", rc == 0 and "pid state" in out, out[-300:])
        rc, out, _ = self.ssh(["stacks"])
        check("stacks, the same", rc == 0 and "closest any stack came" in out, out[-300:])

    def probes(self, sh):
        """What knocks on port 22 all day: it must not cost TCP slots."""
        # connect and close at once, before the server can take it in
        for _ in range(70):
            s = socket.create_connection(("127.0.0.1", SSH_PORT), timeout=10)
            s.close()
        # The burst drained first -- QEMU's user network hands the guest
        # the connections a host client made in its own time -- then
        # connections that only sit there: four get a session, and the
        # rest are reset, the server's count says so, and so do their ends
        # on this side
        def server_view():
            out = sh.run("sshd")
            m = re.search(r"refused (\d+)", out)
            return (int(m.group(1)) if m else -1), out.count("logging in")

        def connections():
            m = re.search(r"connections (\d+)", sh.run("sshd"))
            return int(m.group(1)) if m else -1

        # Drained means the server has seen the last of the burst, not that
        # it has seen none of it yet: "nobody logging in" is just as true
        # before the first probe arrives, and the shell answers faster than
        # the user network delivers -- the stragglers then land in the count
        # below and are taken for the refusals it is looking for. So: nobody
        # logging in, and a connection count that has stopped moving.
        start = time.time()
        seen, quiet_since = connections(), time.time()
        while time.time() - start < 60:
            time.sleep(1)
            now = connections()
            if now != seen or server_view()[1] != 0:
                seen, quiet_since = now, time.time()
            elif time.time() - quiet_since >= 3:
                break
        refused_before = server_view()[0]
        idle = [socket.create_connection(("127.0.0.1", SSH_PORT), timeout=10) for _ in range(6)]
        start = time.time()
        while server_view()[0] - refused_before < 2 and time.time() - start < 30:
            time.sleep(1)
        refused, logging_in = server_view()
        check("connections past the four logging in are refused", refused - refused_before == 2 and logging_in == 4,
              "refused %d, logging in %d" % (refused - refused_before, logging_in))
        reset = 0
        for s in idle:
            s.settimeout(2)
            try:
                while s.recv(4096).startswith(b"SSH-"):
                    pass
                reset += 1
            except ConnectionResetError:
                reset += 1
            except socket.timeout:
                pass
        for s in idle:
            s.close()
        check("the client of each refused one sees its end", reset == 2, "%d of 6" % reset)
        time.sleep(3)
        out = sh.run("tcpstat")
        lingering = sum(out.count(state) for state in ("CLOSE_WAIT", "TIME_WAIT", "FIN_WAIT", "LAST_ACK", "SYN_RCVD"))
        check("no TCP slot left behind by the probes", lingering == 0, out)
        rc, out, _ = self.ssh(["uptime"])
        check("a login after the probes", rc == 0 and uptime_like(out), out)

    def editor(self, sh, slow):
        """edit (modules/tools/edit) through a terminal of the test's own: the size
        the session was told, a resize, a save, a write over a file changed
        meanwhile, and the changes kept when the connection goes."""
        sh.run("wget http://10.0.2.2:%d/edit.ko /edit.ko" % HTTP_PORT)
        check("insmod edit", "loaded" in sh.run("insmod /edit.ko"))
        path = "/etc/edit-test.conf"
        rc, out, _ = self.ssh(["edit", path])
        check("edit with no terminal says it needs one", rc == 0 and "needs a terminal" in out, out)

        def holds(name):
            """What the file at `name` holds -- cat ends what it prints with
            a newline of its own, which is taken off."""
            rc, out, _ = self.ssh(["cat", name])
            return out[:-1] if rc == 0 and out.endswith("\n") else None

        with Terminal(self, ["edit", path], cols=100, rows=30) as e:
            # The status bar on the row before the last of the terminal's
            # own size: the size the session was told reached the command
            check("edit draws at the terminal's size, a new file", e.wait(b"[new]", 30 * slow) and
                  b"\x1b[29;1H" in e.seen and b"\x1b[30;1H" in e.seen, repr(e.seen[-300:]))
            e.type(b"hello\rworld")
            e.type(b"\x13")
            check("^S saves", e.wait(b"saved", 20 * slow), repr(e.seen[-300:]))
            out = holds(path)
            check("the file is what was typed, a newline at the end of a new one", out == "hello\nworld\n", repr(out))

            # Written by someone else meanwhile: asked about, then written over
            sh.run("write %s changed" % path)
            e.since_now()
            e.type(b"!\x13")
            check("a save over a file changed on disk asks first", e.wait(b"Write over it?", 20 * slow),
                  repr(e.seen[-300:]))
            e.type(b"y")
            check("and writes it on a yes", e.wait(b"saved", 20 * slow), repr(e.seen[-300:]))

            # A resize: the window-change reaches the command, which draws the
            # whole screen again at the new size
            e.since_now()
            e.resize(60, 20)
            check("a resize draws everything again, at the new size", e.wait(b"\x1b[2J", 20 * slow) and
                  e.wait(b"\x1b[19;1H", 10 * slow), repr(e.seen[e.mark:][-300:]))
            e.since_now()
            e.type(b"\x11")
            check("^Q leaves, and gives the terminal back", e.exited(20 * slow) and e.wait(b"\x1b[?1049l", 5),
                  repr(e.seen[-200:]))
        out = holds(path)
        check("the file is what was saved", out == "hello\nworld!\n", repr(out))

        with Terminal(self, ["edit", path], cols=80, rows=24) as e:
            e.wait(b"line 1/2", 30 * slow)
            e.type(b"x")
            check("a change shows", e.wait(b"[modified]", 20 * slow), repr(e.seen[-300:]))
            # The connection gone under changes not saved
        start = time.time()
        out = None
        while time.time() - start < 20 * slow:
            out = holds(path + ".save")
            if out is not None and out.startswith("x"):
                break
            time.sleep(1)
        check("changes left when the connection went are in <file>.save", out == "xhello\nworld!\n", repr(out))
        out = holds(path)
        check("and the file is as it was saved", out == "hello\nworld!\n", repr(out))
        sh.run("rmmod edit")
        check("rmmod edit", "edit" not in sh.run("lsmod"))


class Terminal:
    """`ssh -t` on a terminal of the test's own: a pty of a size the test
    sets, and changes -- which ssh tells the server in a window-change --
    typed at as a person types, everything it shows kept."""

    def __init__(self, t, args, cols, rows):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
            os.environ["TERM"] = "xterm-256color"
            os.execvp("ssh", ["ssh", "-p", str(SSH_PORT), "-i", t.key, "-o", "IdentitiesOnly=yes",
                              "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no",
                              "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR", "-t",
                              "root@127.0.0.1"] + args)
        self.seen = b""
        self.mark = 0
        self.status = None

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.kill()
        os.close(self.fd)

    def read(self, timeout):
        """What came within `timeout`: None when nothing did, b"" once ssh is gone."""
        ready, _, _ = select.select([self.fd], [], [], timeout)
        if not ready:
            return None
        try:
            data = os.read(self.fd, 65536)
        except OSError:
            data = b""
        self.seen += data
        return data

    def since_now(self):
        """What `wait` looks for from here on must come after this."""
        self.mark = len(self.seen)

    def wait(self, marker, timeout=30):
        start = time.time()
        while time.time() - start < timeout:
            if marker in self.seen[self.mark:]:
                return True
            if self.read(0.2) == b"":
                break
        return marker in self.seen[self.mark:]

    def type(self, data):
        os.write(self.fd, data)

    def resize(self, cols, rows):
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    def exited(self, timeout):
        start = time.time()
        while time.time() - start < timeout:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid == self.pid:
                self.status = status
                self.pid = 0
                return True
            self.read(0.2)
        return False

    def kill(self):
        if self.pid:
            os.kill(self.pid, signal.SIGKILL)
            os.waitpid(self.pid, 0)
            self.pid = 0


def serve_http(directory):
    handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=directory)
    handler.log_message = lambda *a: None
    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", HTTP_PORT), handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd


def wait_log(log, marker, timeout):
    start = time.time()
    while time.time() - start < timeout:
        text = open(log, errors="replace").read() if os.path.exists(log) else ""
        if marker in text:
            return True
        if "PANIC" in text:
            print(text[-3000:])
            sys.exit("kernel panic")
        time.sleep(0.5)
    return False


def boot(argv, log):
    if os.path.exists(log):
        os.remove(log)
    return subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def kill(p):
    if p is not None and p.poll() is None:
        p.send_signal(signal.SIGTERM)
        try:
            p.wait(10)
        except subprocess.TimeoutExpired:
            p.kill()


def exited(p, timeout):
    """Whether the machine switched itself off: QEMU exits by itself."""
    try:
        p.wait(timeout)
        return True
    except subprocess.TimeoutExpired:
        return False


def aarch64(t):
    image = os.path.join(t.tmp, "root.img")
    log = os.path.join(t.tmp, "serial.log")
    empty = os.path.join(t.tmp, "empty")
    os.makedirs(empty)
    t.mkrootfs(image, empty)
    with open(os.path.join(t.tmp, "big.txt"), "w") as f:
        f.write(("x" * 63 + "\n") * (BIG // 64))
    shutil.copy(pt.module_path("aarch64", "sshd"), os.path.join(t.tmp, "sshd.ko"))
    shutil.copy(pt.module_path("aarch64", "edit"), os.path.join(t.tmp, "edit.ko"))
    serve_http(t.tmp)

    hvf = platform.system() == "Darwin" and platform.machine() == "arm64" and not t.tcg
    accel = ["-accel", "hvf", "-cpu", "host"] if hvf else ["-accel", "tcg", "-cpu", "cortex-a72"]
    slow = 1 if hvf else 4
    argv = ["qemu-system-aarch64", "-M", "virt,gic-version=3", "-smp", "4", "-m", "1024"] + accel + [
        "-kernel", os.path.join(ROOT, "nos-arm64.img"),
        "-append", "dhcp=auto dns=on root=auto udpshell=%d" % SHELL_PORT,
        "-global", "virtio-mmio.force-legacy=false",
        "-drive", "file=%s,format=raw,id=hd,if=none" % image, "-device", "virtio-blk-device,drive=hd",
        "-device", "virtio-net-device,netdev=net0",
        "-netdev", "user,id=net0,hostfwd=udp::%d-:%d,hostfwd=tcp::%d-:22" % (SHELL_PORT, SHELL_PORT, SSH_PORT),
        "-device", "virtio-rng-device", "-serial", "file:" + log, "-display", "none"]

    p = boot(argv, log)
    try:
        if not check("boots", wait_log(log, "boot: complete", 120 * slow)):
            return
        time.sleep(2)
        sh = Shell()
        sh.run("wget http://10.0.2.2:%d/sshd.ko /sshd.ko" % HTTP_PORT)
        sh.run("wget http://10.0.2.2:%d/big.txt /big.txt" % HTTP_PORT)
        out = sh.run("insmod /sshd.ko port=2222")
        check("a module that takes no parameters refuses one, and says so",
              "takes no parameters" in out and "loaded" not in out and "sshd" not in sh.run("lsmod"), out)
        check("insmod", "loaded" in sh.run("insmod /sshd.ko"))
        check("with no keys, sshd start says nobody can log in", "nobody can log in" in sh.run("sshd start"))
        sh.run("sshd stop")
        check("sshd allow", "allowed" in sh.run("sshd allow " + t.pub))
        out = sh.run("sshd start")
        check("sshd start", "listening" in out, out)
        fingerprint = out.split("host key ")[1].split()[0] if "host key " in out else ""

        t.battery(sh)
        t.sftp(sh)
        t.probes(sh)
        t.editor(sh, slow)

        # The keys: listed, denied -- and the login with it refused -- and
        # allowed again, the file read back
        listed = subprocess.run(["ssh-keygen", "-l", "-f", t.key + ".pub"], capture_output=True).stdout.decode()
        own = listed.split()[1] if listed else "?"
        check("sshd keys lists the key", own in sh.run("sshd keys"))
        check("sshd deny", "denied" in sh.run("sshd deny " + own))
        rc, _, err = t.ssh(["uptime"])
        check("a denied key is refused", rc == 255 and "Permission denied" in err, err)
        check("sshd allow it again", "allowed" in sh.run("sshd allow " + t.pub))
        check("sshd keys reload finds it in the file", "1 authorized keys" in sh.run("sshd keys reload"))

        # From one of its own sessions a stop would wait for itself
        rc, out, _ = t.ssh(["sshd", "stop"])
        check("sshd stop from its own session is refused", rc == 0 and "wait for itself" in out, out)

        # A stop from the UDP shell, with a session open
        session = t.ssh_background([], tty=True)
        time.sleep(3 * slow)
        start = time.time()
        out = sh.run("sshd stop")
        check("sshd stop with a session open", "stopped" in out and time.time() - start < 10, out)
        try:
            _, err = session.communicate(timeout=20)
            check("the session was ended, the client told why", session.returncode == 255 and
                  b"the server is stopping" in err, err)
        except subprocess.TimeoutExpired:
            session.kill()
            check("the session was ended, the client told why", False, "still running")

        # rmmod from inside a session: the shell's wait gives out after five
        # seconds, the session sees the stop, and the unload finishes
        sh.run("sshd start")
        t.ssh(["rmmod", "sshd"], timeout=60)
        time.sleep(3)
        check("rmmod sshd from one of its sessions", "sshd" not in sh.run("lsmod"))

        # Loaded again, it knows the keys in the file before any start --
        # and a key denied now stays out of the file for the start after
        sh.run("insmod /sshd.ko")
        check("the keys are read at insmod", own in sh.run("sshd keys"))
        sh.run("sshd allow " + open(t.other + ".pub").read().strip())
        sh.run("rmmod sshd")
        sh.run("insmod /sshd.ko")
        other = subprocess.run(["ssh-keygen", "-l", "-f", t.other + ".pub"], capture_output=True).stdout.decode().split()[1]
        check("sshd deny before any start", "denied" in sh.run("sshd deny " + other))
        sh.run("sshd start")
        rc, _, err = t.ssh(["uptime"], key=t.other)
        check("the key denied before the start stays refused", rc == 255 and "Permission denied" in err, err)
        sh.run("rmmod sshd")

        sh.run("rc add insmod /sshd.ko")
        sh.run("rc add a line to take out")
        sh.run("rc add sshd start")
        check("rc del takes a line out", "take out" not in sh.run("rc del 2"))
        check("rc shows the lines", "sshd start" in sh.run("rc"))
        sh.run("sync")
    finally:
        kill(p)

    known_hosts = os.path.join(t.tmp, "known_hosts")
    p = boot(argv, log)
    try:
        if not check("boots again", wait_log(log, "boot: complete", 120 * slow)):
            return
        check("rc brought sshd back by itself", wait_log(log, "sshd: listening on", 60 * slow))
        rc, out, _ = t.ssh(["uptime"], known_hosts=known_hosts, strict="accept-new")
        check("a login after the reboot", rc == 0 and uptime_like(out), out)
        listed = subprocess.run(["ssh-keygen", "-l", "-f", known_hosts], capture_output=True).stdout.decode()
        check("the same host key", fingerprint != "" and fingerprint in listed, "%s vs %s" % (fingerprint, listed))
        # The module has to go before the filesystems do -- the first unmount,
        # whichever it is -- and then the machine all the way down. The halt's
        # fault in the static destructors that this used to look away from
        # was an exiting CPU leaving its one WFI, gone since 7908bf6: a panic
        # on the way down is a failure like any other now
        t.ssh(["poweroff"], known_hosts=known_hosts)
        down = wait_log(log, "vfs: unmounting /", 60)
        text = open(log, errors="replace").read()
        unloaded = text.find("module: sshd unloaded")
        check("poweroff unloads sshd before the unmount",
              down and 0 <= unloaded < text.find("vfs: unmounting /"))
        check("and switches the machine off, with nothing panicked on the way",
              exited(p, 30 * slow) and "PANIC" not in open(log, errors="replace").read())
    finally:
        kill(p)


def x86_64(t):
    image = os.path.join(t.tmp, "root.img")
    log = os.path.join(t.tmp, "serial.log")
    rootdir = os.path.join(t.tmp, "rootdir")
    os.makedirs(os.path.join(rootdir, "etc", "ssh"))
    shutil.copy(pt.module_path("x86_64", "sshd"), os.path.join(rootdir, "sshd.ko"))
    with open(os.path.join(rootdir, "etc", "ssh", "authorized_keys"), "w") as f:
        f.write("# the test's key\n" + t.pub + "\n")
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# sshd-test.py\ninsmod /sshd.ko\nsshd start\n")
    t.mkrootfs(image, rootdir)

    kvm = os.path.exists("/dev/kvm") and not t.tcg
    argv = ["qemu-system-x86_64", "-display", "none", "-m", "1G", "-smp", "4",
            "-cdrom", os.path.join(ROOT, "nos.iso"), "-serial", "file:" + log,
            "-drive", "file=%s,format=raw,id=drive0,if=none" % image,
            "-device", "virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off",
            "-device", ("virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off" if t.nic == "virtio"
                        else t.nic + ",netdev=net0"),
            "-netdev", "user,id=net0,hostfwd=tcp::%d-:22" % SSH_PORT, "-device", "virtio-rng-pci"]
    if kvm:
        argv += ["-enable-kvm", "-cpu", "host"]

    p = boot(argv, log)
    try:
        if not check("boots", wait_log(log, "boot: complete", 400)):
            return
        check("sshd up by itself, from /etc/rc", t.wait_ssh(300))
        check("rc's output on the console", "sshd: listening on" in open(log, errors="replace").read())
        rc, out, _ = t.ssh([], stdin=b"sshd\rexit\r", tty=True)
        check("a shell with a terminal", rc == 0 and "listening" in out, repr(out[-300:]))
        rc, out, _ = t.ssh(["dmesg", "60", "rc:"])
        check("rc's output in the kernel log", rc == 0 and "rc: /etc/rc done" in out, out)
        data = os.urandom(300000)
        sent, back = os.path.join(t.tmp, "sent.bin"), os.path.join(t.tmp, "back.bin")
        with open(sent, "wb") as f:
            f.write(data)
        rc, out, err = t.sftp_batch(["put %s /sent.bin" % sent, "get /sent.bin %s" % back, "rm /sent.bin"])
        check("sftp: a file in and back out, the same bytes", rc == 0 and os.path.exists(back) and
              open(back, "rb").read() == data, (err + out)[-300:])
    finally:
        kill(p)


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--arch", choices=["aarch64", "x86_64"], default="aarch64")
    p.add_argument("--tcg", action="store_true", help="no HVF or KVM, even where there is one")
    p.add_argument("--keep", action="store_true", help="keep the scratch directory: logs, images, keys")
    p.add_argument("--nic", choices=["virtio", "e1000e", "igb"], default="virtio",
                   help="x86_64's network card: QEMU's 82574L (the e1000e driver) or 82576 (igb) for virtio-net")
    p.add_argument("--ssh-port", type=int, default=SSH_PORT,
                   help="the host port sshd is forwarded to, for a run beside another that holds 2222")
    args = p.parse_args()
    if args.nic != "virtio" and args.arch != "x86_64":
        sys.exit("--nic %s is for x86_64" % args.nic)
    globals()["SSH_PORT"] = args.ssh_port

    tmp = tempfile.mkdtemp(prefix="sshd-test.")
    print("sshd-test: %s, scratch in %s" % (args.arch, tmp), flush=True)
    try:
        t = Test(tmp, args.tcg)
        t.nic = args.nic
        {"aarch64": aarch64, "x86_64": x86_64}[args.arch](t)
    finally:
        if not args.keep:
            shutil.rmtree(tmp, ignore_errors=True)

    print("sshd-test: " + ("FAILED: " + ", ".join(failures) if failures else "passed"))
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
