import type { Locale } from "./i18n";

export type StageStatus = "done" | "ongoing" | "progress" | "planned";

export interface Card {
    title: string;
    text: string;
    href: string;
}

// The case for nos on the home page: what is wrong with the host as it is, and what nos does about it.
export interface Why {
    title: string;
    lead: string;
    stats: { value: string; label: string }[];
    ai: string;
    approachTitle: string;
    approach: { kicker: string; title: string; text: string }[];
    closing: string;
    sourcesTitle: string;
    sources: { label: string; href: string }[];
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
        why: Why;
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

/*
 * What the case on the home page stands on. The CVE counts are NVD's records
 * from the kernel.org CNA by publication date, rejected ones left out, counted
 * on 22 October 2026 (2025: 5,675; 2026 to that day: 7,856; August 1,643,
 * September 2,111; January to March 644); the exploited ones, CISA's catalog
 * of 21 October 2026. nos's own: scripts/sloc.py and scripts/unsafe-count.py.
 */
const whySources = {
    nvd: "https://services.nvd.nist.gov/rest/json/cves/2.0?sourceIdentifier=416baaa9-dc9f-4396-8d5f-8c081fb06d67&pubStartDate=2026-09-01T00:00:00.000Z&pubEndDate=2026-09-30T23:59:59.999Z&noRejected&resultsPerPage=1",
    announce: "https://lore.kernel.org/linux-cve-announce/",
    kev: "https://www.cisa.gov/known-exploited-vulnerabilities-catalog",
    torvalds: "https://lkml.org/lkml/2026/5/17/896",
    fix: "https://git.kernel.org/linus/de845981da67a6b049080c87e605130b0c30adc5",
    tencent: "https://matrix.tencent.com/en/2026/10/12/vghost-cve-2026-74580",
    sloc: "https://github.com/irqlevel/nos/blob/master/scripts/sloc.py",
};

const en: Dictionary = {
    meta: {
        title: "nos — an OS kernel in C++ and Rust with a hypervisor of its own",
        description:
            "nos is an operating system kernel for x86-64 and arm64, written in C++20 and Rust, with its own hypervisor that runs Linux guests. Docs, guides and a blog.",
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
            "nos is an operating system kernel for x86-64 and arm64, written in C++20, Rust and a little assembly. It runs in QEMU, in KVM-based clouds, on Apple Silicon and on real servers. Where we're going: a cloud host that runs Linux virtual machines under its own hypervisor, written in Rust.",
        ctaAbout: "What is nos",
        ctaRun: "Run it in QEMU",
        ctaGithub: "Source on GitHub",
        terminalCaption: "The last lines of a real arm64 boot in QEMU.",
        why: {
            title: "Why another kernel",
            lead: "Linux is written in C. It's a great language, but it trusts the programmer with every bounds check, every object lifetime and every lock, across tens of millions of lines of code. Mistakes are inevitable, and the vulnerabilities now come by the thousand.",
            stats: [
                { value: "7,856", label: "vulnerabilities (CVEs) published for the Linux kernel in 2026, as of 22 October. That's more than in all of 2025 (5,675)." },
                { value: "3,754", label: "of them in August and September alone. The whole first quarter had 644." },
                { value: "8", label: "kernel vulnerabilities added this year to CISA's list of bugs that attackers actually exploit" },
            ],
            ai: "AI has made finding these bugs cheap. In May, Linus Torvalds wrote that the “continued flood of AI reports has basically made the security list almost entirely unmanageable”. In August, a fix co-credited to an AI system closed Vghost (CVE-2026-74580). It was a stale cache in vhost that let a local user become root. With a virtual IOMMU, it also let a virtual machine write outside its memory into the host's QEMU process, breaking exactly the wall a hypervisor is there to keep.",
            approachTitle: "Our approach",
            approach: [
                {
                    kicker: "≈ 90,000 lines",
                    title: "Small enough to check",
                    text: "The whole system (kernel, drivers, network stack, filesystems and hypervisor) is about 90,000 lines of code, not tens of millions. It's small enough to review in full, and much easier to verify.",
                },
                {
                    kicker: "63% Rust · 1.7% of it unsafe",
                    title: "Safe Rust by default",
                    text: "63% of the code is Rust, and only 1.7% of those lines are inside unsafe blocks. They sit in the few crates that talk to the hardware and to the C side, and we recount them on every change. In the rest of the code, out-of-bounds writes, use-after-free and data races simply can't be written. The hypervisor's logic has just 4 unsafe spots in 18,929 lines.",
                },
                {
                    kicker: "4 host fuzzers in CI",
                    title: "Fuzzing as a first-class citizen",
                    text: "Four fuzzers compile the kernel's own code as ordinary programs and hammer it with random input: the hypervisor's devices, the network stack, the filesystems, and the C++ under ASan and UBSan. They run in CI on every change, with long runs in between.",
                },
                {
                    kicker: "UBSan · RUSTUB · W^X",
                    title: "Checked on every change",
                    text: "CI boots builds with undefined-behaviour checks turned on, on both architectures. Every subsystem has its own test, and when something breaks, the kernel stops with a panic that says where, instead of hanging silently.",
                },
            ],
            closing: "Linux doesn't go away: it runs as a guest. What changes is the host underneath, the one layer that must not fail. We want it small enough to check, and built so that whole classes of bugs can't be written in it.",
            sourcesTitle: "Sources",
            sources: [
                { label: "NVD: the kernel.org CNA's CVEs, e.g. September 2026", href: whySources.nvd },
                { label: "linux-cve-announce", href: whySources.announce },
                { label: "CISA KEV catalog", href: whySources.kev },
                { label: "Linus Torvalds, 17 May 2026", href: whySources.torvalds },
                { label: "the Vghost fix (Assisted-by: tencentos-corvus-ai)", href: whySources.fix },
                { label: "Tencent Zhuque Lab on Vghost", href: whySources.tencent },
                { label: "nos: scripts/sloc.py, scripts/unsafe-count.py", href: whySources.sloc },
            ],
        },
        highlightsTitle: "What is inside",
        highlightsLead:
            "Every design decision starts from the same list: no undefined behaviour, no memory corruption, no data races, no deadlocks, and no failure that goes unnoticed.",
        highlights: [
            {
                title: "Two architectures",
                text: "The same kernel runs on x86-64 (booting from BIOS or UEFI) and on arm64. Architecture-specific code sits behind a thin hardware layer, and CI boots both on every change.",
                href: "/docs/boot",
            },
            {
                title: "Rust on top of a C++ core",
                text: "The core (boot, memory management, the scheduler, interrupts) is C++20. Everything above it is Rust: the block layer, the network stack, filesystems, virtio and all drivers. Unsafe code lives in one library, kcore.",
                href: "/docs/rust",
            },
            {
                title: "A hypervisor of its own",
                text: "It supports both AMD-V and Intel VT-x and ships as a module you can load and unload. Unmodified Linux, Alpine and Debian boot as guests with several CPUs, virtio disks and networking through a NAT switch.",
                href: "/hypervisor",
            },
            {
                title: "Networking",
                text: "Its own TCP/IP stack with DHCP and DNS, an HTTPS client (TLS 1.3 via rustls), and an SSH server you log into with an Ed25519 key.",
                href: "/docs/tls",
            },
            {
                title: "Storage",
                text: "An asynchronous block layer with NVMe and virtio drivers, MBR and GPT partitions, read-write ext2, nanofs (our own checksummed filesystem), and a VFS on top.",
                href: "/docs/filesystems",
            },
            {
                title: "Loadable modules",
                text: "Kernel services can be written in Rust as separate .ko files and loaded with insmod while the system runs. Their memory is never writable and executable at the same time.",
                href: "/docs/modules",
            },
            {
                title: "Gates, not hopes",
                text: "CI boots builds with undefined-behaviour checks, every subsystem has a dedicated test, and four fuzzers test the kernel's own code on the host.",
                href: "/docs/testing",
            },
            {
                title: "Failures that are loud",
                text: "When something goes wrong, you find out: readable backtraces, a watchdog for stuck locks, a panic that names the CPUs that stopped answering, a profiler, and netconsole for machines without a serial port.",
                href: "/docs/debug",
            },
        ],
        platformsTitle: "Where it runs",
        platformsLead:
            "We develop in emulators and clouds and test on real hardware. Two of our machines have no screen and no serial port, only the network.",
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
            { name: "Hetzner EX44", note: "Intel CPU, RTL8125 NIC, VT-x guests; reachable only over the network" },
            { name: "Hetzner AX41", note: "AMD Ryzen CPU, Intel I210 NIC, AMD-V guests" },
        ],
        roadmapTitle: "Roadmap",
        roadmapLead:
            "Where we're heading: a cloud server that runs nos directly on the hardware, hosts Linux guests under a Rust hypervisor, and can update itself without stopping them.",
        stages: [
            { title: "Bare metal", text: "nos boots on real servers and can be managed remotely.", status: "done" },
            { title: "Rust strategy", text: "New code is written in Rust, with unsafe kept to a few small crates.", status: "ongoing" },
            { title: "HAL + arm64", text: "A hardware abstraction layer and an arm64 port, done before the hypervisor.", status: "done" },
            { title: "Hypervisor", text: "A Rust hypervisor that boots Linux guests on AMD-V and Intel VT-x.", status: "progress" },
            { title: "Control plane", text: "An HTTP API to create and delete virtual machines and reach their consoles.", status: "planned" },
            { title: "Live update", text: "Replace the host kernel under running guests in less than 100 ms.", status: "planned" },
        ],
        status: { done: "Done", ongoing: "Ongoing", progress: "In progress", planned: "Planned" },
        blogTitle: "From the blog",
        blogAll: "All posts",
        learnTitle: "Learn more",
        learn: [
            {
                title: "Documentation",
                text: "How each part of the system works, from boot to the hypervisor. It's generated from the repository, so it always matches the code.",
                href: "/docs",
            },
            {
                title: "How-to guides",
                text: "Step by step: build nos, run it, connect to it over the network, install it on a server, boot Linux under it, write a module.",
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
            "Step-by-step guides: build nos, run it, connect to it, debug it, install it on a server, boot Linux under its hypervisor, write a module, and run the tests.",
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
            "nos — ядро операционной системы для x86-64 и arm64 на C++20 и Rust с собственным гипервизором, в котором работают гостевые Linux. Документация, руководства и блог.",
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
            "nos — ядро операционной системы для x86-64 и arm64 на C++20, Rust и немного ассемблера. Оно работает в QEMU, в облаках на KVM, на Apple Silicon и на настоящих серверах. Наша цель — облачный хост, который запускает виртуальные машины с Linux под собственным гипервизором на Rust.",
        ctaAbout: "Что такое nos",
        ctaRun: "Запустить в QEMU",
        ctaGithub: "Исходники на GitHub",
        terminalCaption: "Последние строки настоящей загрузки arm64 в QEMU.",
        why: {
            title: "Зачем ещё одно ядро",
            lead: "Linux написан на C. Это отличный язык, но каждую проверку границ, время жизни каждого объекта и каждую блокировку он доверяет программисту, и так на десятках миллионов строк кода. Ошибки неизбежны, и уязвимости теперь находят тысячами.",
            stats: [
                { value: "7 856", label: "уязвимостей (CVE) в ядре Linux опубликовано в 2026 году к 22 октября. Это больше, чем за весь 2025-й (5 675)." },
                { value: "3 754", label: "из них — только за август и сентябрь. За весь первый квартал было 644." },
                { value: "8", label: "уязвимостей ядра CISA в этом году добавило в список тех, что реально используются в атаках" },
            ],
            ai: "ИИ сделал поиск таких ошибок дешёвым. В мае Линус Торвальдс написал, что непрекращающийся поток отчётов от ИИ сделал список рассылки по безопасности почти неуправляемым. В августе исправление, соавтором которого указана ИИ-система, закрыло Vghost (CVE-2026-74580). Это был устаревший кэш в vhost, через который локальный пользователь получал root. А при виртуальном IOMMU гостевая машина могла писать за пределы своей памяти прямо в процесс QEMU на хосте, то есть пробить ту самую стену, которую гипервизор должен держать.",
            approachTitle: "Наш подход",
            approach: [
                {
                    kicker: "≈ 90 000 строк",
                    title: "Достаточно маленькое, чтобы проверить",
                    text: "Вся система (ядро, драйверы, сетевой стек, файловые системы и гипервизор) — это около 90 000 строк кода, а не десятки миллионов. Её можно прочитать целиком, и её гораздо проще верифицировать.",
                },
                {
                    kicker: "63% Rust · 1,7% из них unsafe",
                    title: "Безопасный Rust по умолчанию",
                    text: "63% кода написано на Rust, и только 1,7% этих строк находятся внутри unsafe-блоков. Они собраны в нескольких крейтах, которые общаются с железом и с кодом на C, и мы пересчитываем их при каждом изменении. В остальном коде запись за границы, use-after-free и гонки данных просто невозможно написать. В логике гипервизора всего 4 места с unsafe на 18 929 строк.",
                },
                {
                    kicker: "4 фаззера в CI",
                    title: "Фаззинг на первом месте",
                    text: "Четыре фаззера собирают код ядра как обычные программы и заваливают его случайными данными: устройства гипервизора, сетевой стек, файловые системы и код на C++ под ASan и UBSan. Они запускаются в CI на каждое изменение, а в промежутках идут долгие прогоны.",
                },
                {
                    kicker: "UBSan · RUSTUB · W^X",
                    title: "Каждое изменение под проверкой",
                    text: "В CI на обеих архитектурах загружаются сборки с проверками неопределённого поведения. У каждой подсистемы есть свой тест, а если что-то ломается, ядро останавливается с паникой и говорит, где именно, вместо того чтобы тихо зависнуть.",
                },
            ],
            closing: "Linux никуда не девается: он работает гостем. Меняется хост под ним, единственный слой, который не имеет права упасть. Мы хотим, чтобы он был достаточно маленьким, чтобы его проверить, и устроенным так, чтобы целые классы ошибок в нём нельзя было написать.",
            sourcesTitle: "Источники",
            sources: [
                { label: "NVD: CVE от CNA kernel.org, например за сентябрь 2026", href: whySources.nvd },
                { label: "linux-cve-announce", href: whySources.announce },
                { label: "каталог CISA KEV", href: whySources.kev },
                { label: "Линус Торвальдс, 17 мая 2026", href: whySources.torvalds },
                { label: "исправление Vghost (Assisted-by: tencentos-corvus-ai)", href: whySources.fix },
                { label: "Tencent Zhuque Lab о Vghost", href: whySources.tencent },
                { label: "nos: scripts/sloc.py, scripts/unsafe-count.py", href: whySources.sloc },
            ],
        },
        highlightsTitle: "Что внутри",
        highlightsLead:
            "Каждое решение здесь начинается с одного списка: никакого неопределённого поведения, порчи памяти, гонок данных и взаимоблокировок — и никаких ошибок, которые проходят незамеченными.",
        highlights: [
            {
                title: "Две архитектуры",
                text: "Одно и то же ядро работает на x86-64 (загрузка через BIOS или UEFI) и на arm64. Код, зависящий от архитектуры, спрятан за тонким аппаратным слоем, а CI загружает обе версии на каждое изменение.",
                href: "/docs/boot",
            },
            {
                title: "Rust поверх ядра на C++",
                text: "Основа (загрузка, управление памятью, планировщик, прерывания) написана на C++20. Всё, что выше, — на Rust: блочный слой, сетевой стек, файловые системы, virtio и все драйверы. Небезопасный код собран в одной библиотеке, kcore.",
                href: "/docs/rust",
            },
            {
                title: "Собственный гипервизор",
                text: "Поддерживает и AMD-V, и Intel VT-x и поставляется как модуль, который можно загрузить и выгрузить. Немодифицированные Linux, Alpine и Debian загружаются гостями с несколькими CPU, дисками virtio и сетью через коммутатор с NAT.",
                href: "/hypervisor",
            },
            {
                title: "Сеть",
                text: "Собственный стек TCP/IP с DHCP и DNS, HTTPS-клиент (TLS 1.3 через rustls) и SSH-сервер, в который можно зайти по ключу Ed25519.",
                href: "/docs/tls",
            },
            {
                title: "Хранилище",
                text: "Асинхронный блочный слой с драйверами NVMe и virtio, разделы MBR и GPT, ext2 на чтение и запись, nanofs (наша файловая система с контрольными суммами) и VFS поверх всего этого.",
                href: "/docs/filesystems",
            },
            {
                title: "Загружаемые модули",
                text: "Сервисы ядра можно писать на Rust отдельными .ko-файлами и загружать через insmod прямо в работающую систему. Их память никогда не бывает одновременно доступной на запись и на исполнение.",
                href: "/docs/modules",
            },
            {
                title: "Гейты вместо надежд",
                text: "В CI загружаются сборки с проверками неопределённого поведения, у каждой подсистемы есть свой тест, а четыре фаззера проверяют собственный код ядра на хосте.",
                href: "/docs/testing",
            },
            {
                title: "Отказы, которые слышно",
                text: "Если что-то пошло не так, вы об этом узнаете: понятные бэктрейсы, сторожевой таймер для зависших блокировок, паника с номерами не ответивших CPU, профилировщик и netconsole, чтобы достать лог с машины без последовательного порта.",
                href: "/docs/debug",
            },
        ],
        platformsTitle: "Где работает",
        platformsLead:
            "Мы разрабатываем в эмуляторах и облаках и проверяем на настоящем железе. У двух наших машин нет ни экрана, ни последовательного порта, только сеть.",
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
            { name: "Hetzner EX44", note: "Intel, сетевая карта RTL8125, гости на VT-x; доступ только по сети" },
            { name: "Hetzner AX41", note: "AMD Ryzen, сетевая карта Intel I210, гости на AMD-V" },
        ],
        roadmapTitle: "Дорожная карта",
        roadmapLead:
            "Куда мы идём: облачный сервер, на котором nos работает прямо на железе, запускает гостевые Linux под гипервизором на Rust и умеет обновляться, не останавливая их.",
        stages: [
            { title: "Голое железо", text: "nos загружается на настоящих серверах, и ими можно управлять удалённо.", status: "done" },
            { title: "Стратегия Rust", text: "Новый код пишется на Rust, а unsafe собран в нескольких маленьких крейтах.", status: "ongoing" },
            { title: "HAL + arm64", text: "Слой абстракции оборудования и порт на arm64, сделанные раньше гипервизора.", status: "done" },
            { title: "Гипервизор", text: "Гипервизор на Rust, который загружает гостевые Linux на AMD-V и Intel VT-x.", status: "progress" },
            { title: "Плоскость управления", text: "HTTP API, чтобы создавать и удалять виртуальные машины и подключаться к их консолям.", status: "planned" },
            { title: "Живое обновление", text: "Замена ядра хоста под работающими гостями меньше чем за 100 мс.", status: "planned" },
        ],
        status: { done: "Готово", ongoing: "Постоянно", progress: "В работе", planned: "В планах" },
        blogTitle: "Из блога",
        blogAll: "Все статьи",
        learnTitle: "Что дальше",
        learn: [
            {
                title: "Документация",
                text: "Как устроена каждая часть системы, от загрузки до гипервизора. Она генерируется из репозитория, поэтому всегда совпадает с кодом.",
                href: "/docs",
            },
            {
                title: "Руководства",
                text: "По шагам: собрать nos, запустить, подключиться по сети, поставить на сервер, загрузить под ним Linux, написать модуль.",
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
            "Пошаговые руководства: собрать nos, запустить, подключиться, отладить, поставить на сервер, загрузить Linux под его гипервизором, написать модуль и прогнать тесты.",
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
