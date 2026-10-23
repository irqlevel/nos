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
            en: "How to build nos natively or in Docker, for x86-64 and arm64, with undefined-behaviour checks, plus the disk and system images and BIOS vs. UEFI.",
            ru: "Как собрать nos нативно или в Docker, для x86-64 и arm64, с проверками неопределённого поведения, а также образы дисков и выбор между BIOS и UEFI.",
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
            en: "Putting nos on a server: nos-install writes the release image to a partition, boots it once through the server's own GRUB, and handles updates and rollbacks.",
            ru: "Как поставить nos на сервер: nos-install записывает образ релиза на раздел, один раз загружает его через GRUB сервера и умеет обновлять и откатывать.",
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
            en: "The self-tests that run on every boot, the smoke tests, and the extra tests each subsystem has for problems a quick boot can't catch.",
            ru: "Самотесты, которые идут при каждой загрузке, smoke-тесты и отдельные тесты для каждой подсистемы — на то, что быстрая загрузка не заметит.",
        },
    },
    "kernel-parameters": {
        group: "start",
        title: { en: "Kernel parameters", ru: "Параметры ядра" },
        description: {
            en: "Boot options: smp=, maxcpus=, console=, dhcp=, udpshell=, netconsole=, loglevel= and the rest.",
            ru: "Параметры загрузки: smp=, maxcpus=, console=, dhcp=, udpshell=, netconsole=, loglevel= и остальные.",
        },
    },
    boot: {
        group: "internals",
        title: { en: "Boot", ru: "Загрузка" },
        description: {
            en: "How nos boots on both architectures, from GRUB/Multiboot2 or the Linux Image protocol all the way to boot: complete, and how the other CPUs start.",
            ru: "От GRUB/Multiboot2 и протокола Linux Image до boot: complete на обеих архитектурах и как поднимаются остальные процессоры.",
        },
    },
    paging: {
        group: "internals",
        title: { en: "Paging and memory", ru: "Страничная память" },
        description: {
            en: "How memory works: the early linear map, the real page tables, the page and heap allocators, MMIO, W^X and TLB shootdowns.",
            ru: "Как устроена память: начальное линейное отображение, таблицы страниц, аллокаторы страниц и кучи, MMIO, W^X и TLB shootdown.",
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
            en: "How interrupts are routed: IDT and IOAPIC, MSI-X, GICv3 and the ITS, IPIs, NMIs, IRQ balancing and deferred work.",
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
            en: "How wget https:// works: rustls without the standard library, certificates, and what it costs.",
            ru: "Как работает wget https://: rustls без стандартной библиотеки, сертификаты и во что это обходится.",
        },
    },
    random: {
        group: "internals",
        title: { en: "Randomness", ru: "Случайные числа" },
        description: {
            en: "The ChaCha20 random pool, where its entropy comes from on each machine, and how much timing jitter is worth.",
            ru: "Пул ChaCha20, источники энтропии, которые находятся на каждой машине, и чего стоит джиттер таймингов.",
        },
    },
    rust: {
        group: "internals",
        title: { en: "Rust in the kernel", ru: "Rust в ядре" },
        description: {
            en: "How the Rust crates are layered, how kernel services reach Rust, the safe types that replace unsafe code, and how to write a driver.",
            ru: "Как устроены крейты, как сервис ядра попадает в Rust, типы вместо unsafe, карта kcore и как написать драйвер.",
        },
    },
    filesystems: {
        group: "internals",
        title: { en: "Filesystems", ru: "Файловые системы" },
        description: {
            en: "The root filesystem and root=, the file API, how the ext2 driver writes data, and how to make and check a root image.",
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
            en: "A module that serves an NVMe disk over UDP with zero copies in both directions, and how it compares with Linux.",
            ru: "NVMe-диск, который модуль раздаёт по UDP без копирования в обе стороны, и сравнение с Linux.",
        },
    },
    sshd: {
        group: "internals",
        title: { en: "sshd", ru: "sshd" },
        description: {
            en: "An SSH server as a module: logging in with an Ed25519 key, starting it at boot from /etc/rc, and which algorithms it supports.",
            ru: "SSH-сервер в модуле: вход по ключу Ed25519, запуск при загрузке из /etc/rc, какие алгоритмы он поддерживает.",
        },
    },
    hypervisor: {
        group: "internals",
        title: { en: "The hypervisor", ru: "Гипервизор" },
        description: {
            en: "A type-1 hypervisor written in Rust and shipped as a loadable module: AMD-V and Intel VT-x, Linux guests.",
            ru: "Гипервизор первого типа на Rust в виде загружаемого модуля: AMD-V и Intel VT-x, гостевые Linux.",
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
