import type { Locale } from "./i18n";

export type StageStatus = "done" | "ongoing" | "progress" | "planned";

export interface Card {
    title: string;
    text: string;
    href: string;
}

// Why nos is written the way it is, on the home page: a short lead, then a card for each thing that keeps it safe.
export interface Why {
    title: string;
    lead: string;
    approach: { kicker: string; title: string; text: string }[];
}

// How to put nos on a server, or try it, on the home page: the commands themselves are the page's.
export interface Try {
    title: string;
    lead: string;
    copy: string;
    copied: string;
    rescue: { kicker: string; title: string; text: string; dialog: string; quiet: string; then: string };
    iso: { kicker: string; title: string; text: string; link: string; tryNos: string };
    local: { kicker: string; title: string; text: string };
    more: string;
    moreLink: string;
}

export interface Dictionary {
    meta: {
        title: string;
        description: string;
    };
    nav: {
        home: string;
        about: string;
        hypervisor: string;
        demo: string;
        docs: string;
        howto: string;
        blog: string;
        github: string;
        menu: string;
        skip: string;
        language: string;
        theme: string;
    };
    home: {
        eyebrow: string;
        title: string;
        lead: string;
        ctaAbout: string;
        ctaRun: string;
        ctaDemo: string;
        ctaGithub: string;
        ctaInstall: string;
        terminalCaption: string;
        try: Try;
        why: Why;
        highlightsTitle: string;
        highlights: Card[];
        platformsTitle: string;
        platformsLead: string;
        virtual: string;
        metal: string;
        platformsVirtual: { name: string; note: string }[];
        platformsMetal: { name: string; note: string }[];
        roadmapTitle: string;
        roadmapLead: string;
        stages: { title: string; text: string; status: StageStatus }[];
        status: Record<StageStatus, string>;
        blogTitle: string;
        blogAll: string;
        learnTitle: string;
        learn: Card[];
    };
    blog: {
        title: string;
        description: string;
        allPosts: string;
        newer: string;
        older: string;
        rss: string;
        tags: Record<string, string>;
    };
    docs: {
        title: string;
        description: string;
        groups: Record<string, string>;
        notice: string;
        noticeLink: string;
        viewSource: string;
        allDocs: string;
    };
    howto: {
        title: string;
        description: string;
        allGuides: string;
        guide: string;
    };
    article: {
        onThisPage: string;
        updated: string;
        published: string;
        otherLanguage: string;
    };
    notFound: {
        title: string;
        text: string;
        home: string;
    };
    footer: {
        tagline: string;
        project: string;
        learn: string;
        source: string;
        releases: string;
        rss: string;
        license: string;
    };
}

/*
 * The numbers in the home page's "why" cards are the tree's own, from
 * scripts/sloc.py on 20 November 2026: Rust is 160,401 of the 220,577 lines
 * of code (72.7%), 1,316 of its lines are inside unsafe blocks (0.8%), and
 * hypervisor/hv has 4 unsafe blocks in 22,125 lines. Recount before changing
 * them.
 */
const en: Dictionary = {
    meta: {
        title: "nos — an OS kernel in C++ and Rust with a hypervisor of its own",
        description:
            "nos is an operating system kernel for x86-64 and arm64, written in C++20 and Rust, with its own hypervisor that runs Linux and Windows Server guests. Docs, guides and a blog.",
    },
    nav: {
        home: "Home",
        about: "What is nos",
        hypervisor: "Hypervisor",
        demo: "Demo",
        docs: "Docs",
        howto: "How-to",
        blog: "Blog",
        github: "GitHub",
        menu: "Menu",
        skip: "Skip to content",
        language: "Language",
        theme: "Toggle dark mode",
    },
    home: {
        eyebrow: "x86-64 · arm64 · C++20 · Rust",
        title: "A hypervisor that runs on bare metal",
        lead:
            "nos is installed straight onto a server and runs virtual machines on it. The network stack, storage, hypervisor and management API are all its own, so there is no other OS underneath. It's written in Rust and C++20 and runs on x86-64 and arm64. Linux and Windows Server VMs run on x86-64 hosts only, for now.",
        ctaAbout: "What is nos",
        ctaRun: "Run it in QEMU",
        ctaDemo: "Watch the demo",
        ctaGithub: "Source on GitHub",
        ctaInstall: "Install on a server",
        terminalCaption: "The last lines of a real arm64 boot in QEMU.",
        try: {
            title: "Install it on a server",
            lead: "nos takes a whole disk of an x86-64 server: a 10 GiB root for the system, and the rest of the disk left empty for virtual machines and storage. Pick the way your hosting provider gives you.",
            copy: "Copy",
            copied: "Copied",
            rescue: {
                kicker: "Rescue system",
                title: "One command",
                text: "Boot the server into your provider's rescue system, log in over SSH as root and run:",
                dialog: "A dialog asks which disk to use, how big the root should be and which SSH keys may log in. It offers the server's own network, as the rescue system has it, and the keys root logs in with there. Then it downloads the newest release, checks it against the release's checksums, makes the disk bootable under BIOS and UEFI, and puts nos first in the UEFI boot order, right after network boot. nos's SSH server takes ed25519 keys only.",
                quiet: "The same without questions:",
                then: "If the provider's panel still has the server booting into rescue, switch that back. Then reboot, and about a minute later ssh root@your-server gets you into nos.",
            },
            iso: {
                kicker: "ISO",
                title: "The installer ISO",
                text: "For servers installed from an ISO: mount it as virtual media through IPMI, iDRAC or iLO, or give its link to your provider's install-from-ISO. The same dialog runs on the server's console.",
                link: "Download the ISO",
                tryNos: "Not sure the server suits nos? The menu entry Try nos boots nos straight from the ISO and writes nothing. The screen then shows which disks and network cards nos found.",
            },
            local: {
                kicker: "Your computer",
                title: "Try it without a server",
                text: "Run nos in QEMU on a laptop in a few minutes, or watch the demo first.",
            },
            more: "Step by step, with the commands for doing it by hand and for putting nos on a spare partition next to the system already installed:",
            moreLink: "Install nos on a server",
        },
        why: {
            title: "Why our own kernel",
            lead: "Systems software written in older languages like C gets new vulnerabilities all the time, and most of them are memory bugs. To keep that out of nos as far as we can, we chose modern tools from the start.",
            approach: [
                {
                    kicker: "73% of the code is Rust",
                    title: "Mostly Rust",
                    text: "Drivers, the network stack, filesystems and the hypervisor are written in Rust. In safe Rust you can't write past the end of an array, use memory after it has been freed, or cause a data race.",
                },
                {
                    kicker: "0.8% of Rust lines are unsafe",
                    title: "Little unsafe code",
                    text: "Code marked unsafe, which the compiler can't fully check, is needed only where nos talks to the hardware or to its C++ part. That's under one percent of the Rust, and the hypervisor's logic has just 4 such places in 22,000 lines.",
                },
                {
                    kicker: "fuzzers and sanitizers",
                    title: "Checked on every change",
                    text: "Fuzzers run the kernel's own code on random data, and CI boots both architectures with undefined-behaviour checks on every change. When something breaks, the kernel stops and says where, instead of hanging silently.",
                },
            ],
        },
        highlightsTitle: "What's inside",
        highlights: [
            {
                title: "Hypervisor",
                text: "Runs on Intel VT-x and AMD-V. Alpine, Debian and Ubuntu boot in its VMs with several CPUs, disks and networking. A VM is made from a cloud image with one command, and it can be given a whole GPU.",
                href: "/hypervisor",
            },
            {
                title: "Cluster",
                text: "Several nos servers work together. VM disks are kept as copies on different servers, VMs on different servers can share one network, and a running VM can move to another server without stopping.",
                href: "/blog/live-migration",
            },
            {
                title: "Management panel",
                text: "A web panel and an HTTP API: create a VM, open its console, look after disks, networks and the cluster's servers.",
                href: "/blog/management-panel",
            },
            {
                title: "Networking",
                text: "Its own TCP/IP stack with DHCP and DNS, an HTTPS client on TLS 1.3, and an SSH server with SFTP.",
                href: "/docs/tls",
            },
            {
                title: "Storage",
                text: "An asynchronous block layer, NVMe, SATA and virtio drivers, MBR and GPT partitions, and ext4 and ext2 for reading and writing.",
                href: "/docs/filesystems",
            },
            {
                title: "x86-64 and arm64",
                text: "One kernel for both. Everything that depends on the CPU sits behind a thin layer, and CI boots both on every change. On x86-64, nos boots from BIOS or UEFI.",
                href: "/docs/boot",
            },
            {
                title: "Modules",
                text: "Kernel services are written in Rust as separate .ko files and loaded into the running system with insmod. The SSH server, the API and the hypervisor itself are modules.",
                href: "/docs/modules",
            },
            {
                title: "Debugging",
                text: "Readable backtraces, watchdogs, tracing and a profiler. Servers without a serial port send their log over the network with netconsole.",
                href: "/docs/debug",
            },
        ],
        platformsTitle: "Where it runs",
        platformsLead: "We develop in QEMU, run in the cloud and test on real servers. Five of them we can reach only over the network.",
        virtual: "Virtual",
        metal: "Bare metal",
        platformsVirtual: [
            { name: "QEMU / KVM", note: "x86-64, BIOS and UEFI" },
            { name: "QEMU virt", note: "arm64" },
            { name: "Google Cloud", note: "KVM, virtio" },
            { name: "Yandex Cloud", note: "KVM, virtio" },
        ],
        platformsMetal: [
            { name: "Dell Latitude 5480", note: "a laptop: UEFI, screen console, USB keyboard" },
            { name: "Hetzner EX44", note: "Intel, an RTL8125 NIC, VMs on VT-x" },
            { name: "Hetzner AX41", note: "AMD Ryzen, an Intel I210 NIC, VMs on AMD-V" },
            { name: "ASUS RS500-E6-PS4", note: "two Xeons, BIOS, SATA in IDE mode" },
            { name: "Supermicro X11SSD-F", note: "a Xeon, UEFI, SATA through AHCI, Intel I350" },
            { name: "MSI PRO B760M-P", note: "Raptor Lake, an RTX A2000 given to a VM" },
            { name: "Supermicro X10DRi", note: "two sockets, two NUMA nodes, 256 GiB" },
        ],
        roadmapTitle: "Roadmap",
        roadmapLead: "We're building a cloud server where nos runs right on the hardware, hosts virtual machines and updates itself without stopping them.",
        stages: [
            { title: "Bare metal", text: "nos boots on real servers, and they can be managed remotely.", status: "done" },
            { title: "Rust", text: "New code is written in Rust, with as little unsafe as possible.", status: "ongoing" },
            { title: "HAL and arm64", text: "A thin layer between the code and the CPU, and the arm64 port. Both were done before the hypervisor.", status: "done" },
            { title: "Hypervisor", text: "A Rust hypervisor for VMs on Intel VT-x and AMD-V.", status: "progress" },
            { title: "Management", text: "An HTTP API and a web panel to create and delete VMs and reach their consoles.", status: "progress" },
            { title: "Live update", text: "Update the server's kernel in under 100 ms without stopping its VMs.", status: "planned" },
        ],
        status: { done: "Done", ongoing: "Ongoing", progress: "In progress", planned: "Planned" },
        blogTitle: "From the blog",
        blogAll: "All posts",
        learnTitle: "Learn more",
        learn: [
            {
                title: "Documentation",
                text: "How each part of nos works, from boot to the hypervisor. It's built straight from the repository, so it matches the code.",
                href: "/docs",
            },
            {
                title: "How-to guides",
                text: "Step by step: build nos, run it, connect to it over the network, install it on a server, run a VM, write a module.",
                href: "/howto",
            },
        ],
    },
    blog: {
        title: "Blog",
        description:
            "Stories from building nos: real bugs on real hardware, the hypervisor, Rust in the kernel, and the tests that keep us honest.",
        allPosts: "All posts",
        newer: "Newer",
        older: "Older",
        rss: "RSS feed",
        tags: {
            hardware: "Hardware",
            networking: "Networking",
            drivers: "Drivers",
            hypervisor: "Hypervisor",
            rust: "Rust",
            memory: "Memory",
            safety: "Safety",
            debugging: "Debugging",
            testing: "Testing",
            fuzzing: "Fuzzing",
            qemu: "QEMU",
            arm64: "arm64",
            "x86-64": "x86-64",
            storage: "Storage",
            boot: "Boot",
            deployment: "Deployment",
            performance: "Performance",
            modules: "Modules",
        },
    },
    docs: {
        title: "Documentation",
        description:
            "The nos reference: how each subsystem works and how to build, run, test and debug the kernel. It's generated from the repository's docs/ on every deploy.",
        groups: {
            start: "Start here",
            internals: "How it works",
            tools: "Tools",
            other: "More",
        },
        notice: "",
        noticeLink: "",
        viewSource: "View on GitHub",
        allDocs: "All documentation",
    },
    howto: {
        title: "How-to guides",
        description:
            "Step-by-step guides: build nos, run it, connect to it, debug it, install it on a server, boot Linux under its hypervisor, make a VM from a cloud image, write a module, and run the tests.",
        allGuides: "All guides",
        guide: "Guide",
    },
    article: {
        onThisPage: "On this page",
        updated: "Updated",
        published: "Published",
        otherLanguage: "Читать по-русски",
    },
    notFound: {
        title: "Page not found",
        text: "There is nothing at this address. The page may have moved — try the documentation or the blog.",
        home: "Go home",
    },
    footer: {
        tagline: "An operating system kernel for x86-64 and arm64, written in C++ and Rust, with its own hypervisor.",
        project: "Project",
        learn: "Learn",
        source: "Source",
        releases: "Releases",
        rss: "RSS",
        license: "Code under the MIT license.",
    },
};

const ru: Dictionary = {
    meta: {
        title: "nos — ядро ОС на C++ и Rust с собственным гипервизором",
        description:
            "nos — ядро операционной системы для x86-64 и arm64 на C++20 и Rust с собственным гипервизором, в котором работают гостевые Linux и Windows Server. Документация, руководства и блог.",
    },
    nav: {
        home: "Главная",
        about: "Что такое nos",
        hypervisor: "Гипервизор",
        demo: "Демо",
        docs: "Документация",
        howto: "Руководства",
        blog: "Блог",
        github: "GitHub",
        menu: "Меню",
        skip: "К содержанию",
        language: "Язык",
        theme: "Переключить тёмную тему",
    },
    home: {
        eyebrow: "x86-64 · arm64 · C++20 · Rust",
        title: "Гипервизор, который работает прямо на железе",
        lead:
            "nos ставится прямо на сервер и запускает на нём виртуальные машины. Сеть, хранилище, гипервизор и API для управления у него свои, так что другая ОС под ним не нужна. Написан на Rust и C++20, работает на x86-64 и arm64. Виртуальные машины с Linux и Windows Server пока запускаются только на хостах x86-64.",
        ctaAbout: "Что такое nos",
        ctaRun: "Запустить в QEMU",
        ctaDemo: "Смотреть демо",
        ctaGithub: "Исходники на GitHub",
        ctaInstall: "Поставить на сервер",
        terminalCaption: "Последние строки настоящей загрузки arm64 в QEMU.",
        try: {
            title: "Поставить на сервер",
            lead: "nos занимает целый диск сервера x86-64: корень на 10 ГиБ под систему, а остальное место остаётся пустым под виртуальные машины и хранилище. Выберите способ, который даёт ваш хостинг-провайдер.",
            copy: "Копировать",
            copied: "Скопировано",
            rescue: {
                kicker: "Rescue-система",
                title: "Одна команда",
                text: "Загрузите сервер в rescue-систему провайдера, зайдите по SSH под root и выполните:",
                dialog: "Диалог спросит, какой диск занять, какого размера сделать корень и с какими SSH-ключами пускать. Сеть он предложит ту, что у rescue-системы, то есть настоящий адрес сервера, а ключи те, с которыми туда заходит root. Потом скачает свежий релиз, сверит его с контрольными суммами релиза, сделает диск загрузочным для BIOS и UEFI и поставит nos первым в порядке загрузки UEFI, сразу после сетевой загрузки. SSH-сервер nos принимает только ключи ed25519.",
                quiet: "То же без вопросов:",
                then: "Если в панели провайдера сервер всё ещё загружается в rescue, верните загрузку как было. Потом перезагрузите сервер, и примерно через минуту ssh root@ваш-сервер пустит вас в nos.",
            },
            iso: {
                kicker: "ISO",
                title: "ISO-установщик",
                text: "Для серверов, которые ставят с ISO: подключите его как виртуальный носитель через IPMI, iDRAC или iLO либо дайте ссылку на него в установке с ISO у провайдера. На консоли сервера запустится тот же диалог.",
                link: "Скачать ISO",
                tryNos: "Не уверены, что сервер подойдёт? Пункт меню Try nos загружает nos прямо с ISO и ничего не пишет на диски. На экране будет видно, какие диски и сетевые карты nos нашёл.",
            },
            local: {
                kicker: "Свой компьютер",
                title: "Попробовать без сервера",
                text: "Запустите nos в QEMU на ноутбуке за несколько минут или сначала посмотрите демо.",
            },
            more: "По шагам, с командами для установки вручную и для установки nos на свободный раздел рядом с уже стоящей системой:",
            moreLink: "Как поставить nos на сервер",
        },
        why: {
            title: "Зачем своё ядро",
            lead: "В системных программах на старых языках вроде C постоянно находят новые уязвимости, и большая часть из них связана с памятью. Чтобы в nos такого было как можно меньше, мы с самого начала выбрали современные инструменты.",
            approach: [
                {
                    kicker: "73% кода на Rust",
                    title: "В основном Rust",
                    text: "На Rust написаны драйверы, сеть, файловые системы и гипервизор. В безопасном Rust нельзя записать за границу массива, обратиться к освобождённой памяти или устроить гонку данных.",
                },
                {
                    kicker: "0,8% строк Rust в unsafe",
                    title: "Мало опасного кода",
                    text: "Код с пометкой unsafe, который компилятор не может проверить до конца, нужен только там, где nos работает с железом или с частью на C++. Это меньше процента кода на Rust, а в логике гипервизора таких мест всего 4 на 22 тысячи строк.",
                },
                {
                    kicker: "фаззеры и санитайзеры",
                    title: "Проверка на каждое изменение",
                    text: "Фаззеры гоняют собственный код ядра на случайных данных, а CI на каждое изменение загружает обе архитектуры с проверками неопределённого поведения. Если что-то сломалось, ядро останавливается и пишет, где именно, а не зависает молча.",
                },
            ],
        },
        highlightsTitle: "Что внутри",
        highlights: [
            {
                title: "Гипервизор",
                text: "Работает на Intel VT-x и AMD-V. В его ВМ запускаются Alpine, Debian и Ubuntu с несколькими CPU, дисками и сетью. ВМ делается из облачного образа одной командой, и ей можно отдать видеокарту целиком.",
                href: "/hypervisor",
            },
            {
                title: "Кластер",
                text: "Несколько серверов с nos работают вместе. Диски ВМ хранятся в нескольких копиях на разных серверах, ВМ с разных серверов могут быть в одной сети, а работающую ВМ можно перенести на другой сервер, не останавливая её.",
                href: "/blog/live-migration",
            },
            {
                title: "Панель управления",
                text: "Веб-панель и HTTP API: создать ВМ, открыть её консоль, управлять дисками, сетями и серверами кластера.",
                href: "/blog/management-panel",
            },
            {
                title: "Сеть",
                text: "Свой стек TCP/IP с DHCP и DNS, HTTPS-клиент на TLS 1.3 и SSH-сервер с SFTP.",
                href: "/docs/tls",
            },
            {
                title: "Хранилище",
                text: "Асинхронный блочный слой, драйверы NVMe, SATA и virtio, разделы MBR и GPT, ext4 и ext2 на чтение и запись.",
                href: "/docs/filesystems",
            },
            {
                title: "x86-64 и arm64",
                text: "Одно ядро для обеих архитектур. Всё, что зависит от процессора, отделено тонким слоем, и CI на каждое изменение загружает обе. На x86-64 nos загружается и через BIOS, и через UEFI.",
                href: "/docs/boot",
            },
            {
                title: "Модули",
                text: "Сервисы ядра пишутся на Rust отдельными .ko-файлами и загружаются в работающую систему командой insmod. Так устроены SSH-сервер, API и сам гипервизор.",
                href: "/docs/modules",
            },
            {
                title: "Отладка",
                text: "Понятные бэктрейсы, сторожевые таймеры, трассировка и профилировщик. Серверы без последовательного порта отдают лог по сети через netconsole.",
                href: "/docs/debug",
            },
        ],
        platformsTitle: "Где работает",
        platformsLead: "Разрабатываем в QEMU, запускаем в облаках и проверяем на настоящих серверах. Пять из них доступны нам только по сети.",
        virtual: "Виртуальные",
        metal: "Реальное железо",
        platformsVirtual: [
            { name: "QEMU / KVM", note: "x86-64, BIOS и UEFI" },
            { name: "QEMU virt", note: "arm64" },
            { name: "Google Cloud", note: "KVM, virtio" },
            { name: "Yandex Cloud", note: "KVM, virtio" },
        ],
        platformsMetal: [
            { name: "Dell Latitude 5480", note: "ноутбук: UEFI, консоль на экране, USB-клавиатура" },
            { name: "Hetzner EX44", note: "Intel, сетевая карта RTL8125, ВМ на VT-x" },
            { name: "Hetzner AX41", note: "AMD Ryzen, сетевая карта Intel I210, ВМ на AMD-V" },
            { name: "ASUS RS500-E6-PS4", note: "два Xeon, BIOS, SATA в режиме IDE" },
            { name: "Supermicro X11SSD-F", note: "Xeon, UEFI, SATA через AHCI, Intel I350" },
            { name: "MSI PRO B760M-P", note: "Raptor Lake, видеокарта RTX A2000 отдана ВМ" },
            { name: "Supermicro X10DRi", note: "два сокета, два узла NUMA, 256 ГиБ" },
        ],
        roadmapTitle: "Дорожная карта",
        roadmapLead: "Мы делаем облачный сервер, на котором nos работает прямо на железе, держит виртуальные машины и обновляется, не останавливая их.",
        stages: [
            { title: "Голое железо", text: "nos загружается на настоящих серверах, и ими можно управлять удалённо.", status: "done" },
            { title: "Rust", text: "Новый код пишем на Rust, а unsafe сводим к минимуму.", status: "ongoing" },
            { title: "HAL и arm64", text: "Тонкий слой между кодом и процессором и порт на arm64. Оба сделаны до гипервизора.", status: "done" },
            { title: "Гипервизор", text: "Гипервизор на Rust для ВМ на Intel VT-x и AMD-V.", status: "progress" },
            { title: "Управление", text: "HTTP API и веб-панель, чтобы создавать и удалять ВМ и подключаться к их консолям.", status: "progress" },
            { title: "Живое обновление", text: "Обновить ядро сервера меньше чем за 100 мс, не останавливая его ВМ.", status: "planned" },
        ],
        status: { done: "Готово", ongoing: "Постоянно", progress: "В работе", planned: "В планах" },
        blogTitle: "Из блога",
        blogAll: "Все статьи",
        learnTitle: "Что дальше",
        learn: [
            {
                title: "Документация",
                text: "Как устроена каждая часть nos, от загрузки до гипервизора. Собирается прямо из репозитория, поэтому не расходится с кодом.",
                href: "/docs",
            },
            {
                title: "Руководства",
                text: "По шагам: собрать nos, запустить, подключиться по сети, поставить на сервер, запустить ВМ, написать модуль.",
                href: "/howto",
            },
        ],
    },
    blog: {
        title: "Блог",
        description:
            "Истории о разработке nos: настоящие баги на настоящем железе, гипервизор, Rust в ядре и тесты, которые не дают нам обманывать себя.",
        allPosts: "Все статьи",
        newer: "Новее",
        older: "Старше",
        rss: "RSS-лента",
        tags: {
            hardware: "Железо",
            networking: "Сеть",
            drivers: "Драйверы",
            hypervisor: "Гипервизор",
            rust: "Rust",
            memory: "Память",
            safety: "Безопасность",
            debugging: "Отладка",
            testing: "Тестирование",
            fuzzing: "Фаззинг",
            qemu: "QEMU",
            arm64: "arm64",
            "x86-64": "x86-64",
            storage: "Хранилище",
            boot: "Загрузка",
            deployment: "Развёртывание",
            performance: "Производительность",
            modules: "Модули",
        },
    },
    docs: {
        title: "Документация",
        description:
            "Справочник по nos: как устроена каждая подсистема и как собрать, запустить, протестировать и отладить ядро. Генерируется из каталога docs/ репозитория при каждом развёртывании.",
        groups: {
            start: "С чего начать",
            internals: "Как это устроено",
            tools: "Инструменты",
            other: "Ещё",
        },
        notice:
            "Эта страница пока есть только на английском: документация собирается прямо из каталога docs/ репозитория и поэтому всегда совпадает с кодом. На русском у нас есть",
        noticeLink: "руководства",
        viewSource: "Открыть на GitHub",
        allDocs: "Вся документация",
    },
    howto: {
        title: "Руководства",
        description:
            "Пошаговые руководства: собрать nos, запустить, подключиться, отладить, поставить на сервер, загрузить Linux под его гипервизором, сделать ВМ из облачного образа, написать модуль и прогнать тесты.",
        allGuides: "Все руководства",
        guide: "Руководство",
    },
    article: {
        onThisPage: "На этой странице",
        updated: "Обновлено",
        published: "Опубликовано",
        otherLanguage: "Read in English",
    },
    notFound: {
        title: "Страница не найдена",
        text: "По этому адресу ничего нет. Возможно, страница переехала — загляните в документацию или в блог.",
        home: "На главную",
    },
    footer: {
        tagline: "Ядро операционной системы для x86-64 и arm64 на C++ и Rust — с собственным гипервизором.",
        project: "Проект",
        learn: "Изучить",
        source: "Код",
        releases: "Релизы",
        rss: "RSS",
        license: "Код распространяется под лицензией MIT.",
    },
};

const dictionaries: Record<Locale, Dictionary> = { en, ru };

export function getDictionary(locale: Locale): Dictionary {
    return dictionaries[locale];
}

export function readingTime(minutes: number, locale: Locale): string {
    if (locale === "ru") {
        return `${minutes} мин чтения`;
    }
    return `${minutes} min read`;
}
