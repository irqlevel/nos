#!/usr/bin/env python3
"""End-to-end test of the sshd module (docs/sshd.md), in QEMU, with OpenSSH.

aarch64 (HVF on an Apple Silicon Mac unless --tcg): boots nos-arm64.img from
an ext2 root, loads sshd.ko over the UDP shell, and has the ssh client work
it -- commands, a shell with a terminal and one without, 3 MiB of output
through the client's window with rekeys in the middle, a key it has to
refuse, sessions at once, a stop and an rmmod from inside a session. Then it
puts the module in /etc/rc, reboots, and checks it came back by itself with
the same host key, and that poweroff unloads it before the filesystems go.

x86_64: boots nos.iso from an ext2 root that already carries sshd.ko, an
authorized key and an /etc/rc that starts it, and logs in: no shell is
touched at all.

    scripts/sshd-test.py [--arch aarch64|x86_64] [--tcg] [--keep]

Needs ssh and ssh-keygen, scripts/mkrootfs.sh (mke2fs, or the nos-builder
image), host ports 2222, 8000 and 9000 free, and `make` or `make
ARCH=aarch64` done first.
"""

import argparse
import functools
import http.server
import os
import platform
import re
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

SSH_PORT = 2222
HTTP_PORT = 8000
SHELL_PORT = 9000

# The UDP shell's framing (scripts/udpsh.py)
SHELL_MAGIC = 0x4E4F5348
SHELL_HDR = struct.Struct("!IIHHHH")
SHELL_LAST = 1

BIG = 3 * 1024 * 1024
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
        subprocess.run([os.path.join(HERE, "mkrootfs.sh"), image, "64", directory], check=True,
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


def serve_http(directory):
    handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=directory)
    handler.log_message = lambda *a: None
    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", HTTP_PORT), handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd


def wait_log(log, marker, timeout, panic_ok=False):
    start = time.time()
    while time.time() - start < timeout:
        text = open(log, errors="replace").read() if os.path.exists(log) else ""
        if marker in text:
            return True
        if "PANIC" in text and not panic_ok:
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


def aarch64(t):
    image = os.path.join(t.tmp, "root.img")
    log = os.path.join(t.tmp, "serial.log")
    empty = os.path.join(t.tmp, "empty")
    os.makedirs(empty)
    t.mkrootfs(image, empty)
    with open(os.path.join(t.tmp, "big.txt"), "w") as f:
        f.write(("x" * 63 + "\n") * (BIG // 64))
    shutil.copy(os.path.join(ROOT, "out", "aarch64", "modules", "sshd.ko"), os.path.join(t.tmp, "sshd.ko"))
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
        check("insmod", "loaded" in sh.run("insmod /sshd.ko"))
        check("with no keys, sshd start says nobody can log in", "nobody can log in" in sh.run("sshd start"))
        sh.run("sshd stop")
        check("sshd allow", "allowed" in sh.run("sshd allow " + t.pub))
        out = sh.run("sshd start")
        check("sshd start", "listening" in out, out)
        fingerprint = out.split("host key ")[1].split()[0] if "host key " in out else ""

        t.battery(sh)
        t.probes(sh)

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
        # arm64's halt panics late in __cxa_finalize (TaskTable::~TaskTable),
        # after the unmount, and did before sshd: what matters here is that
        # the module went before the filesystems did
        t.ssh(["poweroff"], known_hosts=known_hosts)
        check("poweroff unloads sshd before the unmount",
              wait_log(log, "unmounting / (ext2)", 60, panic_ok=True)
              and "module: sshd unloaded" in open(log, errors="replace").read())
    finally:
        time.sleep(2)
        kill(p)


def x86_64(t):
    image = os.path.join(t.tmp, "root.img")
    log = os.path.join(t.tmp, "serial.log")
    rootdir = os.path.join(t.tmp, "rootdir")
    os.makedirs(os.path.join(rootdir, "etc", "ssh"))
    shutil.copy(os.path.join(ROOT, "out", "x86_64", "modules", "sshd.ko"), os.path.join(rootdir, "sshd.ko"))
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
            "-device", "virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off",
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
    finally:
        kill(p)


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--arch", choices=["aarch64", "x86_64"], default="aarch64")
    p.add_argument("--tcg", action="store_true", help="no HVF or KVM, even where there is one")
    p.add_argument("--keep", action="store_true", help="keep the scratch directory: logs, images, keys")
    args = p.parse_args()

    tmp = tempfile.mkdtemp(prefix="sshd-test.")
    print("sshd-test: %s, scratch in %s" % (args.arch, tmp), flush=True)
    try:
        t = Test(tmp, args.tcg)
        {"aarch64": aarch64, "x86_64": x86_64}[args.arch](t)
    finally:
        if not args.keep:
            shutil.rmtree(tmp, ignore_errors=True)

    print("sshd-test: " + ("FAILED: " + ", ".join(failures) if failures else "passed"))
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
