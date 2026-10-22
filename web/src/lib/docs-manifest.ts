import type { Locale } from "./i18n";

export type DocGroup = "start" | "internals" | "tools" | "other";

export interface DocInfo {
    group: DocGroup;
    title: Record<Locale, string>;
    description: Record<Locale, string>;
}

/*
 * How the repository's docs/ are grouped and named on the site -- the same
 * grouping as docs/README.md, with a Russian title and summary for each, since
 * the pages themselves are English. A file in docs/ that is not listed here
 * still gets a page: it is put under "other" with the title of its first heading.
 */
export const docsManifest: Record<string, DocInfo> = {
    features: {
        group: "start",
        title: { en: "Features", ru: "Возможности" },
        description: {
            en: "Everything the kernel does today, by subsystem.",
            ru: "Всё, что ядро умеет сегодня, по подсистемам.",
        },
    },
    build: {
        group: "start",
        title: { en: "Build", ru: "Сборка" },
        description: {
            en: "Native and Docker builds, arm64, the undefined-behaviour flavours, the disk image, the system image, BIOS vs. UEFI.",
            ru: "Сборка нативно и в Docker, arm64, сборки с проверками неопределённого поведения, образ диска, системный образ, BIOS и UEFI.",
        },
    },
    run: {
        group: "start",
        title: { en: "Run", ru: "Запуск" },
        description: {
            en: "QEMU on x86-64 (KVM, TCG, OVMF) and arm64 (HVF), Google Cloud.",
            ru: "QEMU на x86-64 (KVM, TCG, OVMF) и arm64 (HVF), Google Cloud.",
        },
    },
    install: {
        group: "start",
        title: { en: "Install", ru: "Установка" },
        description: {
            en: "nos on a server: the release's system image poured onto a partition with nos-install, booted once through the server's own GRUB, updated, rolled back.",
            ru: "nos на сервере: системный образ релиза на разделе через nos-install, однократная загрузка через GRUB сервера, обновление и откат.",
        },
    },
    "real-hardware": {
        group: "start",
        title: { en: "Real hardware", ru: "Реальное железо" },
        description: {
            en: "The Dell Latitude 5480 laptop and the Hetzner EX44 (Intel) and AX41 (AMD) servers.",
            ru: "Ноутбук Dell Latitude 5480 и серверы Hetzner EX44 (Intel) и AX41 (AMD).",
        },
    },
    debug: {
        group: "start",
        title: { en: "Debug", ru: "Отладка" },
        description: {
            en: "GDB, and getting a log off a box with no serial port.",
            ru: "GDB и как достать лог с машины без последовательного порта.",
        },
    },
    testing: {
        group: "start",
        title: { en: "Tests and gates", ru: "Тесты и гейты" },
        description: {
            en: "The self-tests every boot runs, the smoke boots, and the gate each subsystem has for what a smoke boot cannot notice.",
            ru: "Самотесты каждой загрузки, smoke-загрузки и гейт каждой подсистемы — для того, чего smoke-загрузка не заметит.",
        },
    },
    "kernel-parameters": {
        group: "start",
        title: { en: "Kernel parameters", ru: "Параметры ядра" },
        description: {
            en: "smp=, maxcpus=, console=, dhcp=, udpshell=, netconsole=, loglevel= and the rest.",
            ru: "smp=, maxcpus=, console=, dhcp=, udpshell=, netconsole=, loglevel= и остальные.",
        },
    },
    boot: {
        group: "internals",
        title: { en: "Boot", ru: "Загрузка" },
        description: {
            en: "GRUB/Multiboot2 and the Linux Image protocol to boot: complete, on both architectures, and how the APs come up.",
            ru: "От GRUB/Multiboot2 и протокола Linux Image до boot: complete на обеих архитектурах и как поднимаются остальные процессоры.",
        },
    },
    paging: {
        group: "internals",
        title: { en: "Paging and memory", ru: "Страничная память" },
        description: {
            en: "The bootstrap linear map, the real page table, the free list, the allocators, MMIO and W^X, TLB shootdown.",
            ru: "Начальное линейное отображение, настоящая таблица страниц, список свободных страниц, аллокаторы, MMIO и W^X, TLB shootdown.",
        },
    },
    scheduler: {
        group: "internals",
        title: { en: "Scheduler", ru: "Планировщик" },
        description: {
            en: "Per-CPU run queues, the context switch, preemption, blocking and load balancing.",
            ru: "Очереди задач на каждом CPU, переключение контекста, вытеснение, блокировки и балансировка нагрузки.",
        },
    },
    interrupts: {
        group: "internals",
        title: { en: "Interrupts", ru: "Прерывания" },
        description: {
            en: "The IDT and IOAPIC, MSI-X, GICv3 and the ITS, IPIs, NMI, IRQ balancing, deferred work.",
            ru: "IDT и IOAPIC, MSI-X, GICv3 и ITS, IPI, NMI, балансировка IRQ, отложенная работа.",
        },
    },
    profiler: {
        group: "internals",
        title: { en: "Profiler", ru: "Профилировщик" },
        description: {
            en: "Sampling on a performance counter or the tick, and how to read a profile report.",
            ru: "Сэмплирование по счётчику производительности или по тику и как читать отчёт profile.",
        },
    },
    tls: {
        group: "internals",
        title: { en: "HTTPS", ru: "HTTPS" },
        description: {
            en: "How wget https:// works: the transport seam, rustls in no_std, certificates, and what it costs.",
            ru: "Как работает wget https://: шов транспорта, rustls в no_std, сертификаты и во что это обходится.",
        },
    },
    random: {
        group: "internals",
        title: { en: "Randomness", ru: "Случайные числа" },
        description: {
            en: "The ChaCha20 pool, the entropy sources each machine turns out to have, and what timing jitter is worth.",
            ru: "Пул ChaCha20, источники энтропии, которые находятся на каждой машине, и чего стоит джиттер таймингов.",
        },
    },
    rust: {
        group: "internals",
        title: { en: "Rust in the kernel", ru: "Rust в ядре" },
        description: {
            en: "How the crates are layered, how a kernel service reaches Rust, the types that stand in for unsafe, a map of kcore, writing a driver.",
            ru: "Как устроены крейты, как сервис ядра попадает в Rust, типы вместо unsafe, карта kcore и как написать драйвер.",
        },
    },
    filesystems: {
        group: "internals",
        title: { en: "Filesystems", ru: "Файловые системы" },
        description: {
            en: "The root filesystem and root=, the file API, what the ext2 driver writes and how, making and checking a root image.",
            ru: "Корневая ФС и root=, файловый API, что и как пишет драйвер ext2, создание и проверка корневого образа.",
        },
    },
    modules: {
        group: "internals",
        title: { en: "Loadable modules", ru: "Загружаемые модули" },
        description: {
            en: "Kernel services in Rust, built into .ko files and put into a running kernel with insmod: writing, building, loading.",
            ru: "Сервисы ядра на Rust в виде .ko, загружаемые в работающее ядро через insmod: как написать, собрать и загрузить.",
        },
    },
    netblk: {
        group: "internals",
        title: { en: "netblk", ru: "netblk" },
        description: {
            en: "An NVMe disk served over UDP by a module, zero-copy both ways, and how Linux compares.",
            ru: "NVMe-диск, который модуль раздаёт по UDP без копирования в обе стороны, и сравнение с Linux.",
        },
    },
    sshd: {
        group: "internals",
        title: { en: "sshd", ru: "sshd" },
        description: {
            en: "An SSH server in a module: logging in with an Ed25519 key, starting it at boot from /etc/rc, what it speaks.",
            ru: "SSH-сервер в модуле: вход по ключу Ed25519, запуск при загрузке из /etc/rc, какие алгоритмы он поддерживает.",
        },
    },
    hypervisor: {
        group: "internals",
        title: { en: "The hypervisor", ru: "Гипервизор" },
        description: {
            en: "A type-1 hypervisor as Rust crates and a module that can be taken back out: AMD-V and Intel VT-x, Linux guests.",
            ru: "Гипервизор первого типа из крейтов Rust и выгружаемого модуля: AMD-V и Intel VT-x, гостевые Linux.",
        },
    },
    "udp-shell": {
        group: "tools",
        title: { en: "UDP remote shell", ru: "Удалённая оболочка по UDP" },
        description: {
            en: "Run shell commands over the network with scripts/udpsh.py.",
            ru: "Команды оболочки по сети через scripts/udpsh.py.",
        },
    },
    netconsole: {
        group: "tools",
        title: { en: "Netconsole", ru: "Netconsole" },
        description: {
            en: "Stream the kernel log over UDP to scripts/netconsole.py.",
            ru: "Лог ядра по UDP в scripts/netconsole.py.",
        },
    },
    "shell-commands": {
        group: "tools",
        title: { en: "Shell commands", ru: "Команды оболочки" },
        description: {
            en: "The full command reference.",
            ru: "Полный справочник команд.",
        },
    },
    "project-layout": {
        group: "tools",
        title: { en: "Project layout", ru: "Структура проекта" },
        description: {
            en: "Where things live in src/.",
            ru: "Где что лежит в src/.",
        },
    },
};

export const docGroups: DocGroup[] = ["start", "internals", "tools", "other"];
