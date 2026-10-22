import type { Locale } from "./i18n";

export type StageStatus = "done" | "ongoing" | "progress" | "planned";

export interface Card {
    title: string;
    text: string;
    href: string;
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
        ctaGithub: string;
        terminalCaption: string;
        highlightsTitle: string;
        highlightsLead: string;
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

const en: Dictionary = {
    meta: {
        title: "nos — an OS kernel in C++ and Rust with a hypervisor of its own",
        description:
            "nos is a hobby x86-64 and arm64 operating system kernel in C++20 and Rust with a type-1 hypervisor that boots Linux guests. Docs, how-to guides and a blog.",
    },
    nav: {
        home: "Home",
        about: "What is nos",
        hypervisor: "Hypervisor",
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
        title: "An OS kernel built to be safe by design",
        lead:
            "nos is a hobby operating system kernel for x86-64 and arm64, written in C++20, Rust and assembly. It boots in QEMU, on KVM clouds, on Apple Silicon and on bare-metal servers — and it is growing into a cloud node that runs Linux guests under its own Rust hypervisor.",
        ctaAbout: "What is nos",
        ctaRun: "Run it in QEMU",
        ctaGithub: "Source on GitHub",
        terminalCaption: "The end of a real arm64 boot log under QEMU.",
        highlightsTitle: "What is inside",
        highlightsLead:
            "No undefined behaviour, no memory corruption, no data races, no deadlocks, no silent failure: every design here is picked for that first.",
        highlights: [
            {
                title: "Two architectures",
                text: "x86-64 with Multiboot2 under BIOS and UEFI, and arm64 on QEMU virt — one kernel behind a HAL, both booted in CI on every change.",
                href: "/docs/boot",
            },
            {
                title: "Rust over C++ fundamentals",
                text: "Boot, paging, allocators, the scheduler and interrupts are C++20. The block layer, network stack, filesystems, virtio and every driver are Rust, with unsafe confined to kcore.",
                href: "/docs/rust",
            },
            {
                title: "A hypervisor of its own",
                text: "AMD-V and Intel VT-x backends in a module that can be unloaded. Unmodified Linux, Alpine and Debian boot as guests with several CPUs, virtio disks and a NATed switch.",
                href: "/hypervisor",
            },
            {
                title: "Networking",
                text: "ARP, IPv4, DHCP, DNS, UDP and TCP, an HTTP client with TLS 1.3 through rustls, and an SSH server you log into with an Ed25519 key.",
                href: "/docs/tls",
            },
            {
                title: "Storage",
                text: "An asynchronous block layer with NVMe and virtio drivers, MBR and GPT, ext2 read-write, the checksummed nanofs, and a VFS over them.",
                href: "/docs/filesystems",
            },
            {
                title: "Loadable modules",
                text: "Kernel services in Rust, built into .ko files, bound against an export table generated from the FFI declarations, loaded with insmod — never writable and executable at once.",
                href: "/docs/modules",
            },
            {
                title: "Gates, not hopes",
                text: "UBSan and checked-unsafe builds booted in CI, a gate of its own for each subsystem, and four host fuzzers built from the kernel's own sources.",
                href: "/docs/testing",
            },
            {
                title: "Failures that are loud",
                text: "Symbolized backtraces, a lock watchdog, a TLB shootdown that names the CPUs that never answered, a PMU profiler, and netconsole for machines with no serial port.",
                href: "/docs/debug",
            },
        ],
        platformsTitle: "Where it runs",
        platformsLead:
            "Developed against emulators and clouds, and proven on real machines — two of which have no console but the network.",
        virtual: "Virtual",
        metal: "Bare metal",
        platformsVirtual: [
            { name: "QEMU / KVM", note: "x86-64, BIOS and UEFI" },
            { name: "Apple Silicon", note: "QEMU virt with HVF" },
            { name: "Google Cloud", note: "KVM, virtio" },
            { name: "Yandex Cloud", note: "KVM, virtio" },
        ],
        platformsMetal: [
            { name: "Dell Latitude 5480", note: "UEFI, framebuffer console, USB keyboard" },
            { name: "Hetzner EX44", note: "Intel, RTL8125, VT-x guests; network-only console" },
            { name: "Hetzner AX41", note: "AMD Ryzen, Intel I210, AMD-V guests" },
        ],
        roadmapTitle: "Roadmap",
        roadmapLead:
            "From a kernel that boots on servers to a bare-metal cloud node: no Linux on the host, Linux guests under a Rust hypervisor, live update underneath them.",
        stages: [
            { title: "Bare metal", text: "nos boots and stays manageable on real server hardware.", status: "done" },
            { title: "Rust strategy", text: "New code in Rust, unsafe confined to a few small crates.", status: "ongoing" },
            { title: "HAL + arm64", text: "A portability layer and arm64 under QEMU and HVF, before the hypervisor.", status: "done" },
            { title: "Hypervisor", text: "A Rust hypervisor on AMD-V and VT-x boots Linux guests.", status: "progress" },
            { title: "Control plane", text: "An HTTP API creates, destroys and consoles virtual machines.", status: "planned" },
            { title: "Live update", text: "The host kernel replaced under running guests in under 100 ms.", status: "planned" },
        ],
        status: { done: "Done", ongoing: "Ongoing", progress: "In progress", planned: "Planned" },
        blogTitle: "From the blog",
        blogAll: "All posts",
        learnTitle: "Learn more",
        learn: [
            {
                title: "Documentation",
                text: "How each subsystem works, from boot to the hypervisor — built from the repository, so it describes the code as it is.",
                href: "/docs",
            },
            {
                title: "How-to guides",
                text: "Build it, run it, talk to it over the network, put it on a server, boot Linux under it, write a module.",
                href: "/howto",
            },
        ],
    },
    blog: {
        title: "Blog",
        description:
            "Stories from building nos: real bugs on real hardware, the hypervisor, Rust in the kernel, and the gates that keep it honest.",
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
            "The reference for nos, built from the docs/ directory of the repository on every deploy: how each subsystem works, how to build, run, test and debug it.",
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
            "Task-by-task guides: build nos, run it, talk to it, debug it, put it on a server, boot Linux under its hypervisor, write a module, run the gates.",
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
        tagline: "A hobby x86-64 and arm64 kernel in C++20 and Rust, and a hypervisor of its own.",
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
            "nos — хобби-ядро операционной системы для x86-64 и arm64 на C++20 и Rust с гипервизором первого типа, который загружает гостевые Linux. Документация, руководства и блог.",
    },
    nav: {
        home: "Главная",
        about: "Что такое nos",
        hypervisor: "Гипервизор",
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
        title: "Ядро ОС, безопасное по построению",
        lead:
            "nos — хобби-ядро операционной системы для x86-64 и arm64 на C++20, Rust и ассемблере. Оно загружается в QEMU, в облаках на KVM, на Apple Silicon и на выделенных серверах — и вырастает в облачный узел, который запускает гостевые Linux под собственным гипервизором на Rust.",
        ctaAbout: "Что такое nos",
        ctaRun: "Запустить в QEMU",
        ctaGithub: "Исходники на GitHub",
        terminalCaption: "Конец настоящего лога загрузки arm64 в QEMU.",
        highlightsTitle: "Что внутри",
        highlightsLead:
            "Ни неопределённого поведения, ни порчи памяти, ни гонок, ни взаимоблокировок, ни тихих отказов: каждое решение здесь выбирается прежде всего ради этого.",
        highlights: [
            {
                title: "Две архитектуры",
                text: "x86-64 с Multiboot2 под BIOS и UEFI и arm64 на QEMU virt — одно ядро за слоем HAL, обе архитектуры загружаются в CI на каждое изменение.",
                href: "/docs/boot",
            },
            {
                title: "Rust поверх фундамента на C++",
                text: "Загрузка, страничная память, аллокаторы, планировщик и прерывания — на C++20. Блочный слой, сетевой стек, файловые системы, virtio и все драйверы — на Rust, а unsafe собран в kcore.",
                href: "/docs/rust",
            },
            {
                title: "Собственный гипервизор",
                text: "Бэкенды AMD-V и Intel VT-x в выгружаемом модуле. Немодифицированные Linux, Alpine и Debian загружаются гостями — с несколькими CPU, дисками virtio и коммутатором с NAT.",
                href: "/hypervisor",
            },
            {
                title: "Сеть",
                text: "ARP, IPv4, DHCP, DNS, UDP и TCP, HTTP-клиент с TLS 1.3 через rustls и SSH-сервер со входом по ключу Ed25519.",
                href: "/docs/tls",
            },
            {
                title: "Хранилище",
                text: "Асинхронный блочный слой с драйверами NVMe и virtio, MBR и GPT, ext2 на чтение и запись, nanofs с контрольными суммами и VFS поверх всего этого.",
                href: "/docs/filesystems",
            },
            {
                title: "Загружаемые модули",
                text: "Сервисы ядра на Rust в виде .ko: связываются с таблицей экспорта, сгенерированной из FFI-деклараций, загружаются через insmod и никогда не бывают записываемыми и исполняемыми одновременно.",
                href: "/docs/modules",
            },
            {
                title: "Гейты вместо надежд",
                text: "Сборки с UBSan и проверками unsafe загружаются в CI, у каждой подсистемы свой гейт, а четыре фаззера на хосте собраны из исходников самого ядра.",
                href: "/docs/testing",
            },
            {
                title: "Отказы, которые слышно",
                text: "Символизированные бэктрейсы, сторожевой таймер блокировок, TLB shootdown, который называет не ответившие CPU, профилировщик на PMU и netconsole для машин без последовательного порта.",
                href: "/docs/debug",
            },
        ],
        platformsTitle: "Где работает",
        platformsLead:
            "Разрабатывается в эмуляторах и облаках, проверяется на реальных машинах — у двух из них нет никакой консоли, кроме сети.",
        virtual: "Виртуальные",
        metal: "Реальное железо",
        platformsVirtual: [
            { name: "QEMU / KVM", note: "x86-64, BIOS и UEFI" },
            { name: "Apple Silicon", note: "QEMU virt с HVF" },
            { name: "Google Cloud", note: "KVM, virtio" },
            { name: "Yandex Cloud", note: "KVM, virtio" },
        ],
        platformsMetal: [
            { name: "Dell Latitude 5480", note: "UEFI, консоль на фреймбуфере, USB-клавиатура" },
            { name: "Hetzner EX44", note: "Intel, RTL8125, гости на VT-x; консоль — только сеть" },
            { name: "Hetzner AX41", note: "AMD Ryzen, Intel I210, гости на AMD-V" },
        ],
        roadmapTitle: "Дорожная карта",
        roadmapLead:
            "От ядра, которое загружается на серверах, к облачному узлу на голом железе: без Linux на хосте, с гостевыми Linux под гипервизором на Rust и с живым обновлением под ними.",
        stages: [
            { title: "Голое железо", text: "nos загружается на реальных серверах и остаётся управляемым.", status: "done" },
            { title: "Стратегия Rust", text: "Новый код — на Rust, unsafe заперт в нескольких маленьких крейтах.", status: "ongoing" },
            { title: "HAL + arm64", text: "Слой переносимости и arm64 под QEMU и HVF — раньше гипервизора.", status: "done" },
            { title: "Гипервизор", text: "Гипервизор на Rust на AMD-V и VT-x загружает гостевые Linux.", status: "progress" },
            { title: "Плоскость управления", text: "HTTP API создаёт и удаляет виртуальные машины и открывает их консоли.", status: "planned" },
            { title: "Живое обновление", text: "Ядро хоста заменяется под работающими гостями быстрее чем за 100 мс.", status: "planned" },
        ],
        status: { done: "Готово", ongoing: "Постоянно", progress: "В работе", planned: "В планах" },
        blogTitle: "Из блога",
        blogAll: "Все статьи",
        learnTitle: "Что дальше",
        learn: [
            {
                title: "Документация",
                text: "Как устроена каждая подсистема, от загрузки до гипервизора. Собирается из репозитория, поэтому описывает код таким, какой он есть.",
                href: "/docs",
            },
            {
                title: "Руководства",
                text: "Собрать, запустить, подключиться по сети, поставить на сервер, загрузить под ним Linux, написать модуль.",
                href: "/howto",
            },
        ],
    },
    blog: {
        title: "Блог",
        description:
            "Истории о разработке nos: настоящие баги на настоящем железе, гипервизор, Rust в ядре и гейты, которые не дают выдать желаемое за действительное.",
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
            "Справочник по nos, который собирается из каталога docs/ репозитория при каждом развёртывании: как устроена каждая подсистема, как собрать, запустить, протестировать и отладить ядро.",
        groups: {
            start: "С чего начать",
            internals: "Как это устроено",
            tools: "Инструменты",
            other: "Ещё",
        },
        notice:
            "Эта страница документации пока есть только на английском. Документация собирается прямо из каталога docs/ репозитория, поэтому всегда соответствует коду. По-русски написаны",
        noticeLink: "руководства",
        viewSource: "Открыть на GitHub",
        allDocs: "Вся документация",
    },
    howto: {
        title: "Руководства",
        description:
            "Пошаговые руководства: собрать nos, запустить, подключиться, отлаживать, поставить на сервер, загрузить Linux под его гипервизором, написать модуль, прогнать гейты.",
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
        tagline: "Хобби-ядро для x86-64 и arm64 на C++20 и Rust — и собственный гипервизор.",
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
