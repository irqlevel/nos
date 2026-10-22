/*
 * The tail of a real boot of the arm64 build under QEMU, as nos-arm64.log
 * recorded it: each line's prefix is the CPU, the seconds since boot and
 * where the trace was printed from -- the last part shown only where the
 * screen is wide enough for it. Nothing here is made up for the page.
 */
const log: { at: string; where: string; text: string; ok?: boolean }[] = [
    { at: "0:1.408961:", where: "hello(),lib.rs,71:", text: "rust_test: passed" },
    { at: "0:1.409463:", where: "Init(),softirq.cpp,61:", text: "SoftIrq: initialized" },
    { at: "0:1.409711:", where: "block(),lib.rs,106:", text: "part: 0 partitions on 1 disks" },
    { at: "0:1.409935:", where: "fs::rootfs(),rootfs.rs,110:", text: "MountRootFs: mounted ramfs on / (rw)" },
    { at: "0:1.410281:", where: "fs::nanofs(),nanofs.rs,326:", text: "nanofs: bad magic 0x0" },
    { at: "0:1.410464:", where: "fs::rootfs(),rootfs.rs,163:", text: "MountRootFs: mounted procfs on /proc (ro)" },
    { at: "0:1.410664:", where: "net::tcp(),tcp.rs,418:", text: "tcp: initialized, up to 64 connections" },
    { at: "0:1.411045:", where: "net::udp_shell(),udp_shell.rs,148:", text: "udpshell: started on port 9000" },
    { at: "0:1.411231:", where: "BpStartupArm(),main_arm64.cpp,337:", text: "boot: complete", ok: true },
];

// The shell's banner, as cmd.cpp prints it.
const banner = ["  _   _  ___  ____", " | \\ | |/ _ \\/ ___|", " |  \\| | | | \\___ \\", " | |\\  | |_| |___) |", " |_| \\_|\\___/|____/"];

const after = ["DHCP auto on eth0...", "DHCP ip: 10.0.2.15", "DNS resolver started, server: 10.0.2.3"];

export function BootTerminal({ caption }: { caption: string }) {
    return (
        <figure className="w-full min-w-0">
            <div className="terminal overflow-hidden rounded-xl">
                <div className="flex items-center gap-2 border-b border-white/5 px-4 py-2.5" aria-hidden="true">
                    <span className="size-2.5 rounded-full bg-[#ff5f57]" />
                    <span className="size-2.5 rounded-full bg-[#febc2e]" />
                    <span className="size-2.5 rounded-full bg-[#28c840]" />
                    <span className="ml-2 font-mono text-[0.7rem] text-[#6b7280]">nos-arm64.log</span>
                </div>
                <pre className="overflow-x-auto px-4 py-4 font-mono text-[0.7rem] leading-[1.65] sm:text-[0.76rem]">
                    <code>
                        <span className="t-accent">$</span> ./scripts/qemu-arm64.sh{"\n"}
                        <span className="t-dim">…</span>
                        {"\n"}
                        {log.map((line) => (
                            <span key={line.at}>
                                <span className="t-dim hidden sm:inline">{line.at}</span>
                                <span className="t-dim hidden 2xl:inline">{line.where}</span>
                                <span className="t-dim hidden sm:inline"> </span>
                                <span className={line.ok ? "t-ok" : undefined}>{line.text}</span>
                                {"\n"}
                            </span>
                        ))}
                        {"\n"}
                        <span className="t-accent">{banner.join("\n")}</span>
                        {"\n\n"}
                        {after.join("\n")}
                        {"\n"}
                        <span className="t-accent">$</span> <span className="cursor">█</span>
                    </code>
                </pre>
            </div>
            <figcaption className="mt-3 text-center text-xs text-muted">{caption}</figcaption>
        </figure>
    );
}
