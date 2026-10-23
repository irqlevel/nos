---
title: "Linux-гость под гипервизором nos"
description: "Соберите маленькое ядро Linux и initramfs на BusyBox, загрузите hv.ko в nos и запускайте Linux-гостей: вводите команды, подключайте к сети, останавливайте."
order: 6
updated: 2026-10-22
---

Гипервизор nos — это загружаемый модуль `hv.ko`. К концу руководства он будет запускать настоящих Linux-гостей внутри nos, а сам nos будет работать в QEMU:

- собранное вами ядро, загруженное один раз на заданное время командой `hv boot`;
- долгоживущих гостей, запущенных командой `hv start`: вы вводите команды в их консоль, даёте им диск и сеть и останавливаете их;
- Alpine Linux в том виде, в каком он поставляется.

Что понадобится:

- **x86-64.** Linux-гости работают под nos только на x86-64.
- **Расширение виртуализации процессора, которое видит nos.** В QEMU есть два варианта:
  - **TCG**, программный эмулятор QEMU, работает на любом хосте, включая Mac. С `-cpu max` он эмулирует AMD-V с вложенной трансляцией страниц (nested paging), то есть с аппаратной трансляцией физических адресов гостя. Это медленно: гость эмулируется дважды, и загрузка до шелла занимает минуты.
  - **KVM** на Linux-хосте с включённой вложенной виртуализацией (`nested=1` у `kvm_amd` или `kvm_intel`). С `-enable-kvm -cpu host` nos получает AMD-V или Intel VT-x самого хоста, и всё работает намного быстрее. В TCG нет VT-x, поэтому бэкенд гипервизора для Intel работает только под KVM или на настоящем железе.
- **`nos.iso` и `hv.ko` из одной сборки или одного релиза.** Ядро отвергает модуль, собранный под другой интерфейс ядра.
- **Клон репозитория nos**: понадобятся его скрипты.
- **x86-64-машина или контейнер с Linux для сборки гостя** и исходники Linux 6.6 или новее. Гейты проекта (его тестовые скрипты) используют 6.18.
- **QEMU 9.2 или новее**, если под TCG нужны гости с несколькими CPU.

Команды ниже рассчитаны на один рабочий каталог, в котором лежат клон nos (`nos/`) и исходники Linux (`linux/`). Рядом с ними вы создадите `guest/` и `rootdir/`.

Как гипервизор устроен внутри, рассказано на [странице гипервизора](/hypervisor) и в [The hypervisor](/docs/hypervisor).

## 1. Соберите ядро гостя

Ядро гостя — обычный, ничем не изменённый 64-битный `bzImage` Linux. Начните с минимальной конфигурации `tinyconfig` и включите то, что предоставляет виртуальная машина гипервизора. В каталоге `linux/`:

```sh
make ARCH=x86_64 tinyconfig
./scripts/config \
    -e 64BIT -e PRINTK -e EARLY_PRINTK -e TTY -e SERIAL_8250 -e SERIAL_8250_CONSOLE \
    -e BLK_DEV_INITRD -e RD_GZIP -e BINFMT_ELF -e BINFMT_SCRIPT -e PROC_FS -e SYSFS -e DEVTMPFS \
    -e MULTIUSER -e POSIX_TIMERS -e FUTEX -e EPOLL -e SIGNALFD -e TIMERFD -e EVENTFD -e FILE_LOCKING \
    -e SMP -e HYPERVISOR_GUEST -e X86_X2APIC -e X86_MPPARSE --set-val NR_CPUS 16 \
    -e PCI -e PCI_MSI -e VIRTIO_MENU -e VIRTIO_PCI -e VIRTIO_PCI_LEGACY \
    -e BLOCK -e BLK_DEV -e VIRTIO_BLK -e EXT4_FS \
    -e NET -e INET -e PACKET -e UNIX -e IP_PNP -e NETDEVICES -e NET_CORE -e VIRTIO_NET
make ARCH=x86_64 olddefconfig
make ARCH=x86_64 -j"$(nproc)" bzImage
```

Ядро появится в `linux/arch/x86/boot/bzImage`. Не опускайте `ARCH=x86_64`: на x86-64-хосте простой `make tinyconfig` настраивает 32-битное ядро. В Debian или Ubuntu для сборки нужны `gcc make bc flex bison libelf-dev libssl-dev python3 xz-utils`.

Для чего эти опции:

| Опции | Для чего |
|---|---|
| `64BIT` … `SERIAL_8250_CONSOLE` | 64-битное ядро, которое пишет в эмулируемый последовательный порт 8250, то есть в консоль гостя |
| `BLK_DEV_INITRD` … `DEVTMPFS` | initramfs в gzip, ELF-программы и скрипты, `/proc`, `/sys` и devtmpfs |
| `MULTIUSER` … `FILE_LOCKING` | то, на что рассчитывают апплеты BusyBox. Важнее всего `POSIX_TIMERS`: `ping` из BusyBox отмеряет интервалы через `alarm()` и без таймеров после первого ответа ждёт вечно |
| `SMP` … `NR_CPUS 16` | гости с несколькими CPU (`cpus=`): x2APIC, которому нужен `HYPERVISOR_GUEST` или `IRQ_REMAP`, таблица MP и до 16 CPU |
| `PCI` … `EXT4_FS` | диски: legacy virtio поверх PCI, MSI-X для гостей с несколькими CPU и ext4 |
| `NET` … `VIRTIO_NET` | сеть гостей: TCP/IP, пакетные сокеты, автонастройка через `ip=` и virtio-net |

Это тот же набор опций, что и у ядра, которое загружает гейт проекта для Linux-гостей. `hv stop` нажимает кнопку питания гостя. Чтобы гость её замечал, добавьте перед `olddefconfig` ещё `-e ACPI -e ACPI_BUTTON -e X86_PM_TIMER -e INPUT -e INPUT_EVDEV`. Чтобы гость по этой кнопке выключался, в нём должен работать `acpid` из BusyBox с обработчиком, который выключает систему ([ACPI](/docs/hypervisor#acpi)). Без ACPI гость кнопку не замечает, и `hv stop` останавливает его сразу.

## 2. Соберите initramfs на BusyBox

initramfs состоит из статического BusyBox и скрипта `/init`. Какой именно BusyBox взять, важно по трём причинам:

- **Его редактор строки должен спрашивать у терминала позицию курсора** (`ESC [ 6 n`). Модуль вводит строку в гостя только после того, как увидел этот вопрос в приглашении.
- **Нужен `httpd`**: он понадобится в сетевом примере ниже.
- **Его `exec` должен запускать скрипт без строки `#!` через шелл**, как того требует `udhcpc -s`.

Статический BusyBox 1.36.1 из Ubuntu 24.04 подходит по всем трём пунктам. В `busybox-static` из Alpine нет `httpd`, и такие скрипты он не запускает, а BusyBox из официального Docker-образа `busybox` позицию курсора не спрашивает.

Возьмите бинарник из Ubuntu, напишите скрипт init и упакуйте архив:

```sh
mkdir -p guest && cd guest
docker run --rm --platform linux/amd64 -v "$PWD:/out" ubuntu:24.04 sh -c \
    'apt-get update -qq && apt-get install -y -qq busybox-static && cp /usr/bin/busybox /out/'
cat > init <<'EOF'
#!/bin/busybox sh
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
echo "nos-guest: init is up"
dmesg -n 1
exec sh
EOF
cat > initramfs.list <<'EOF'
dir /dev 0755 0 0
nod /dev/console 0600 0 0 c 5 1
dir /bin 0755 0 0
dir /proc 0755 0 0
dir /sys 0755 0 0
dir /mnt 0755 0 0
file /bin/busybox busybox 0755 0 0
file /init init 0755 0 0
EOF
../linux/usr/gen_init_cpio initramfs.list | gzip -9 > initrd
cd ..
```

В самой Ubuntu 24.04 Docker не нужен: установите `busybox-static` и скопируйте `/usr/bin/busybox`. `gen_init_cpio` — утилита самого ядра для сборки архива. Она собирается вместе с `bzImage` и создаёт узел устройства без прав root.

> Не убирайте `dmesg -n 1` из init. Если ядро напечатает строку после приглашения шелла, модуль этого приглашения не увидит, и тогда каждый `hv exec` будет заканчиваться неудачей.

## 3. Подготовьте корневую файловую систему nos

Соберите nos в клоне репозитория. После сборки там будут `nos.iso`, `out/x86_64/modules/hv.ko` и `out/x86_64/modules/sshd.ko` ([Сборка nos](/howto/build)):

```sh
cd nos && ./scripts/build-iso-docker.sh && cd ..
```

Вместо этого можно взять `nos.iso`, `hv-x86_64.ko` и `sshd-x86_64.ko` из одного [релиза](https://github.com/irqlevel/nos/releases). Скопируйте модули под именами `hv.ko` и `sshd.ko`, а в команде QEMU ниже используйте `nos.iso` из того же релиза.

Соберите всё, что нужно nos, в `rootdir/` и сделайте из этого каталога образ ext2:

```sh
mkdir -p rootdir
cp nos/out/x86_64/modules/hv.ko nos/out/x86_64/modules/sshd.ko rootdir/
cp linux/arch/x86/boot/bzImage guest/initrd rootdir/
nos/scripts/mkrootfs.sh root.img 256 rootdir
```

`mkrootfs.sh` даёт файловой системе метку `nos`. ISO загружает nos с параметром `root=auto`, а он монтирует файловую систему с этой меткой как `/` на чтение и запись. На Mac скрипт запускает `mkfs.ext2` в контейнере `nos-builder`, который создал `build-iso-docker.sh`. Всё, чем будут пользоваться гости, должно лежать в `rootdir/` в момент создания образа. Разделы про диск и Alpine ниже добавляют файлы: кладите их в `rootdir/` и перед следующей загрузкой снова запускайте `mkrootfs.sh`. Файл размером до 20 МиБ можно и скачать изнутри nos командой `wget <url> <path>`.

## 4. Загрузите nos

```sh
qemu-system-x86_64 -nographic -m 2G -smp 4 -cpu max \
    -cdrom nos/nos.iso \
    -drive file=root.img,format=raw,id=drive0,if=none \
    -device virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off \
    -device virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off \
    -netdev user,id=net0,hostfwd=tcp::2222-:22,hostfwd=tcp::8080-:8080
```

На Linux-хосте с KVM и вложенной виртуализацией замените `-cpu max` на `-enable-kvm -cpu host`.

- **`-cpu max`** даёт AMD-V из TCG с вложенной трансляцией страниц. Модель CPU, которую QEMU берёт по умолчанию, сообщает о SVM (так AMD называет AMD-V) без неё, а гипервизору она нужна.
- **`-nographic`** выводит последовательную консоль nos, а с ней и шелл, в ваш терминал. `Ctrl-A X` завершает QEMU.
- **Два проброса портов** ведут порт хоста 2222 на sshd в nos, а порт хоста 8080 на порт 8080 в nos, которым ниже пользуется `hv forward`.

Подробнее о запуске nos в QEMU рассказано в руководстве [Запуск nos в QEMU](/howto/run-in-qemu).

## 5. Загрузите гипервизор

Введите в шелле nos:

```sh
insmod /hv.ko
hv info
hv on
```

`hv info` перечисляет возможности расширения виртуализации, по одной на строку. `hv on` включает расширение на всех работающих CPU, а `hv on 2` включил бы его только на CPU 2. Последнюю строку `hv on` печатает, спросив сами процессоры, на каких из них расширение включено:

```text
$ insmod /hv.ko
module: hv loaded at 0xFFFF800031000000
$ hv on
hv: turned on for cpu 0-3
hv: AMD-V (SVM) on for cpu 0-3 of 0-3
```

`hv help` перечисляет все подкоманды. `hv off` снова выключает расширение, но отказывается это делать, пока на указанном CPU работает гость, запущенный через `hv start`. `rmmod hv` сначала останавливает всех гостей, а затем выключает расширение на всех CPU.

## 6. Гость на минуту: `hv boot`

```sh
hv boot /bzImage initrd=/initrd secs=60 input=id\n cmdline=console=ttyS0 nolapic
```

`hv boot` потоком читает bzImage и initrd из файлов nos в память гостя и запускает гостя на vCPU на `secs` секунд. Как только у гостя появится шелл, в него вводится текст из `input=` (`\n` — перевод строки). Затем `hv boot` печатает консоль гостя и сообщает, чем закончился запуск. Под TCG отчёт выглядит так (пример из документации проекта, с сокращениями; строки, которые печатает ваш init, будут другими):

```text
  --- ttyS0 ---
[    0.000000] Linux version 6.18.53 ...
[    0.000000] NX (Execute Disable) protection: active
...
[    0.540000] Run /init as init process
...
BusyBox v1.36.1 built-in shell (ash)
~ # id
uid=0 gid=0
~ #
  --- end ttyS0 ---
```

Опции `hv boot` и `hv start`:

| Опция | Что делает |
|---|---|
| `mem=MiB` | Память гостя в МиБ: по умолчанию 256, минимум 64. |
| `secs=N` | Сколько секунд `hv boot` держит гостя запущенным: по умолчанию 60, максимум 600. |
| `initrd=path` | initramfs, файл в файловой системе nos. |
| `input=...` | Текст, который нужно ввести в шелл гостя, как только шелл появится. |
| `cmdline=...` | Командная строка ядра гостя. Она забирает весь остаток строки, поэтому ставьте её последней. По умолчанию это `console=ttyS0 nolapic` для гостя с одним CPU и `console=ttyS0` для нескольких. nos добавляет частоту TSC хоста (`tsc_early_khz=`), если строка её ещё не задаёт и частота известна. |

Пока работает `hv boot`, консоль гостя построчно попадает и в лог ядра nos, в виде строк `hvguest| ...`. Консоль гостя, запущенного через `hv start`, попадает туда, только если передать `hv start` опцию `log`.

## 7. Долгоживущие гости

`hv start` собирает того же гостя, отдаёт его отдельной задаче vCPU и сразу возвращает управление. Дальше вы работаете с гостем остальными командами `hv`: из консоли, по SSH или через UDP-шелл. Команды ниже взяты из прогона гейта проекта под TCG, где гость выходит в шелл примерно через секунду после старта:

```text
$ hv start /bzImage initrd=/initrd cmdline=console=ttyS0 nolapic
hv: vm 0 started on cpu 3 -- /bzImage, 256 MiB, 1 cpu, cmdline "console=ttyS0 nolapic"
$ hv exec 0 secs=60 id
... the rest of its boot ...
~ # id
uid=0 gid=0
~ #
$ hv exec 0 uname -r
uname -r
6.18.53
~ #
$ hv send 0 echo nos$((6*7))nos\n
hv: vm 0: 20 bytes queued
$ hv wait 0 nos42nos
hv: vm 0 printed "nos42nos", 58 ms in
$ hv stop 0
hv: vm 0 stopped -- on request -- nothing in it listens to its power button
```

| Команда | Что делает |
|---|---|
| `hv list` | Перечисляет запущенных гостей: работает ли каждый, останавливается или чем закончился, на каких CPU хоста он работает, его память, время работы и адрес, число перезапусков, а также счётчики выходов, прерываний, остановок (hlt) и «пинков» (kicks). Выходом (VM exit) называется момент, когда процессор покидает гостя и возвращает управление гипервизору, а «пинком» — прерывание, которое выбивает работающий vCPU из гостя. |
| `hv exec <id> [secs=N] <line>` | Вводит строку и печатает ответ до приглашения, которое появится после неё (по умолчанию ждёт 10 с). Строка, введённая, пока гость ещё грузится, дожидается его шелла. |
| `hv send <id> <text>` | Вводит текст сразу, как в терминале (`\n` — перевод строки). Работает и в приглашении входа, где `exec` прождал бы впустую. |
| `hv wait <id> [secs=N] [boot=N] <text>` | Ждёт, пока в консоли текущей загрузки появится `text`. С `boot=N` сначала дожидается N-го перезапуска гостя. |
| `hv console <id> [bytes=N]` | Печатает конец консоли гостя без управляющих последовательностей. |
| `hv restart <id>` | Заново загружает гостя из его файлов, работающего или остановленного. |
| `hv stop <id\|all> [secs=N]` | Нажимает кнопку питания гостя и даёт ему `secs` секунд (по умолчанию 30; `0` — не спрашивать), чтобы выключиться самому. Затем останавливает гостя и сообщает, чем всё закончилось. |

Используйте id, который напечатал `hv start`: пока модуль загружен, id повторно не выдаются. Гость, который сам себя сбрасывает, останавливается, и причиной указывается сброс. Если же гость запущен с `restart`, его каждый раз загружают снова, но не чаще пяти раз в минуту.

### Живая консоль по SSH: `hv attach`

`hv attach` открывает живую консоль гостя. Сначала она показывает последний килобайт вывода, затем всё, что гость печатает, а каждая нажатая клавиша сразу уходит в гостя. Для этого нужен интерактивный сеанс с терминалом, а такой даёт только SSH. Запустите в шелле nos sshd со своим ключом:

```sh
insmod /sshd.ko
sshd allow ssh-ed25519 AAAA... you@host
sshd start
```

Затем подключитесь со своей машины:

```sh
ssh -t -p 2222 root@127.0.0.1 hv attach 0
```

`Ctrl-]` отключает вас от консоли, а гость продолжает работать. О входе в nos подробнее рассказано в руководстве [Подключение к работающему nos](/howto/connect).

## 8. Дайте гостю диск

`disk=<path>` даёт гостю диск virtio, данные которого хранятся в файле на файловой системе nos. Создайте образ ext4 с файлом внутри, положите его в `rootdir/` и пересоздайте `root.img`:

```sh
mkdir -p content && echo hello > content/hello.txt
mke2fs -q -F -t ext4 -d content rootdir/disk.img 32M
nos/scripts/mkrootfs.sh root.img 256 rootdir
```

На Mac нет `mke2fs`, поэтому выполните эту строку в контейнере `nos-builder`. `mke2fs -d` читает расширенные атрибуты, а bind mount из macOS их не отдаёт. Поэтому сначала скопируйте каталог с содержимым внутрь контейнера, как это делает `scripts/mkrootfs.sh`:

```sh
docker run --rm --platform linux/amd64 -v "$PWD:/w" -w /w nos-builder \
    bash -c 'cp -r content /tmp/content && mke2fs -q -F -t ext4 -d /tmp/content rootdir/disk.img 32M'
```

Снова загрузите nos и гипервизор (шаги 4 и 5), затем:

```sh
hv start /bzImage initrd=/initrd disk=/disk.img cmdline=console=ttyS0 nolapic
hv exec 0 secs=60 mount -t devtmpfs devtmpfs /dev
hv exec 0 mount /dev/vda /mnt
hv exec 0 cat /mnt/hello.txt
```

- **Имена и размер.** У гостя может быть до четырёх опций `disk=`, они по порядку становятся `vda`, `vdb` и так далее. Размер диска равен размеру его файла: гость пишет в этих пределах и никогда не увеличивает файл.
- **Один пишет или многие читают.** `disk=path:ro` делает диск для гостя доступным только на чтение. Образ может либо писать один гость, либо читать сколько угодно гостей. Запуск, который нарушил бы это правило, отвергается, и в сообщении назван владелец: `hv: vm 1 not started -- /disk.img: vm 0's disk writes it`.
- **Сброс кэша доходит до диска.** Когда гость сбрасывает кэш диска (flush), nos синхронизирует файловую систему, на которой лежит образ.

## 9. Подключите гостя к сети

Опция `net` команды `hv start` даёт гостю сетевую карту virtio на коммутаторе, общем с другими гостями и с nos. Первый порт получает адрес 10.0.100.2, следующий — 10.0.100.3 и так далее. Свой адрес ядро гостя получает через `ip=`. У самого nos адрес 10.0.100.1. Запустите двух гостей:

```sh
hv start /bzImage initrd=/initrd net cmdline=console=ttyS0 nolapic
hv start /bzImage initrd=/initrd net cmdline=console=ttyS0 nolapic
hv exec 0 secs=60 ping -c 3 10.0.100.1
hv exec 0 ping -c 3 10.0.100.3
ping 10.0.100.2
```

Первые два ping выполняются в госте 0: до nos и до второго гостя. Последнюю команду выполняет сам nos: это его собственный `ping` через виртуальную сетевую карту `hv0`.

**Достучаться до гостя снаружи поможет `hv forward`.** Он перенаправляет порт nos на порт гостя. Поднимите веб-страницу в госте 1 и пробросьте на неё порт 8080 nos:

```sh
hv exec 1 mkdir -p /www
hv exec 1 echo hello from a guest > /www/index.html
hv exec 1 httpd -p 80 -h /www
hv forward add 8080 1 80
```

nos ответит `hv: port 8080 forwarded to vm 1, 10.0.100.3:80`. QEMU пробрасывает порт хоста 8080 в nos, поэтому страницу можно открыть со своей машины:

```sh
curl http://127.0.0.1:8080/index.html
```

**Во внешний мир гости выходят через NAT в nos**, с адреса самого nos. На DHCP-запрос гостя коммутатор отвечает сам: выдаёт адрес порта, указывает nos как маршрутизатор и сообщает DNS-сервер nos. В initramfs на BusyBox нет `/etc/resolv.conf`. Чтобы имена разрешались, направьте его на DNS-сервер, который ядру передал `ip=`:

```sh
hv exec 0 mkdir -p /etc && ln -sf /proc/net/pnp /etc/resolv.conf
```

NAT не пускает гостей в частные сети, на link-local-адреса (на них отвечает сервер метаданных облака), в общее адресное пространство carrier-grade NAT и в подсеть, через которую сам nos выходит наружу. В user-сети QEMU в эту подсеть попадает и хост с адресом 10.0.2.2. Команда `nat allow 10.0.2.2/32` открывает к нему доступ гостям, а `nat` показывает, что делает NAT. Подробности есть в [A network](/docs/hypervisor#a-network).

## 10. Гость с несколькими CPU

`cpus=N` даёт гостю N процессоров, не больше 16. Каждый из них работает как отдельная задача vCPU на своём CPU хоста. Ядро гостя должно быть Linux 6.6 или новее с `SMP`, `X86_X2APIC` и `X86_MPPARSE`; в рецепте выше всё это есть. Для такого гостя командная строка по умолчанию не содержит `nolapic`. А `noapic` nos добавляет сам, потому что у виртуальной машины нет IO-APIC, если не передать `ioapic`. Вот сокращённый прогон на одном из серверов проекта:

```text
$ hv start /bzImage mem=256 cpus=2 initrd=/initrd cmdline=earlyprintk=serial,ttyS0,115200 console=ttyS0 no_timer_check
hv: vm 0 started on cpus 3,2 -- /bzImage, 256 MiB, 2 cpus, cmdline "earlyprintk=serial,ttyS0,115200 console=ttyS0 no_timer_check"
$ hv list
vm 0  running  cpus 3,2  256 MiB  0 s  exits 0  irq 0  hlt 0  kicks 0  /bzImage
$ hv exec 0 grep -e LOC -e RES -e CAL /proc/interrupts
LOC:        266        223   Local timer interrupts
RES:         11         15   Rescheduling interrupts
CAL:        125        144   Function call interrupts
```

- **Каждому vCPU нужен свой CPU хоста.** Два процессора одного гостя никогда не делят один CPU хоста, поэтому у гостя не может быть больше процессоров, чем CPU, на которых `hv on` включил расширение.
- **Гость получает ядра целиком.** По умолчанию (`hv cores own`) ядра гостя принадлежат только ему, и CPU других гостей на них нет. Гость, которому понадобилось бы ядро, занятое другим гостем, отвергается, и сообщение объясняет почему. `hv cores shared` разрешает гостям делить ядра: это подходит для машины, где все гости ваши. `hv cores` показывает, кто какие CPU занимает.
- **Под TCG используйте QEMU 9.2 или новее.** В версиях до 9.2 TCG не пропускает через вложенную таблицу страниц обращения гостя, у которого выключена страничная адресация. Второй CPU гостя стартует в реальном режиме, где её нет, поэтому он работал бы прямо в памяти nos и портил бы её. KVM и настоящее железо всегда делали это правильно.

## 11. Дистрибутив: Alpine

ISO Alpine в варианте `virt` загружается как есть. Ядро и initramfs вы извлекаете из ISO, а сам ISO становится диском гостя, доступным только на чтение. Скачайте `alpine-virt-<version>-x86_64.iso` с сайта Alpine, затем извлеките файлы и разложите их по местам (`xorriso` есть в большинстве дистрибутивов):

```sh
mkdir -p rootdir/alpine
xorriso -osirrox on -indev alpine-virt-3.24.2-x86_64.iso -extract /boot/vmlinuz-virt rootdir/alpine/vmlinuz-virt
xorriso -osirrox on -indev alpine-virt-3.24.2-x86_64.iso -extract /boot/initramfs-virt rootdir/alpine/initramfs-virt
cp alpine-virt-3.24.2-x86_64.iso rootdir/alpine/alpine.iso
chmod 644 rootdir/alpine/*
nos/scripts/mkrootfs.sh root.img 256 rootdir
```

Загрузите nos и гипервизор и запустите гостя:

```sh
hv start /alpine/vmlinuz-virt mem=512 initrd=/alpine/initramfs-virt disk=/alpine/alpine.iso:ro net restart cmdline=console=ttyS0 nolapic modules=loop,squashfs,sd-mod,usb-storage
hv wait 0 secs=600 login:
hv send 0 root\n
hv exec 0 cat /etc/alpine-release; uname -r
```

initramfs Alpine загружает `virtio_blk`, находит ISO на `vda` и ставит в tmpfs базовую систему из пакетов на ISO. Затем OpenRC, система инициализации Alpine, доводит загрузку до приглашения входа на `ttyS0`. getty (программа, которая выводит это приглашение) позицию курсора не спрашивает, поэтому входите через `hv send`: эта команда вводит текст сразу. После первого `send` и `exec` вводит строки сразу.

Когда гость подключён к коммутатору, initramfs Alpine настраивает `eth0` по `ip=` и записывает DNS-сервер nos в `resolv.conf`, так что `apk` добирается до своих зеркал через NAT. `reboot` в госте сбрасывает его, а опция `restart` загружает снова. Чтобы дождаться приглашения входа в следующей загрузке, используйте `hv wait 0 boot=1 login:`. Загружается и облачный образ Debian, см. [A distribution](/docs/hypervisor#a-distribution).

## На сервере

На сервере, установленном из системного образа ([Установка nos на выделенный сервер](/howto/install-on-a-server)), модуль уже лежит в `/lib/modules/hv.ko`, поэтому шаг 5 начинается с `insmod /lib/modules/hv.ko`. Файлы гостя положите на раздел nos из Linux сервера до загрузки nos или скачайте их изнутри nos через `wget`. На серверах проекта гости работают под AMD-V на Ryzen 5 3600 и под VT-x на i5-13500 ([On real hardware](/docs/hypervisor#on-real-hardware)).

## Если что-то пошло не так

**`hv: vm 0: the guest has not read the line -- it is not at a prompt; the line stays queued`.** Модуль так и не увидел, что шелл гостя спросил позицию курсора. Проверьте, что ваш BusyBox её спрашивает (шаг 2) и что в init есть `dmesg -n 1`. В приглашении входа пользуйтесь `hv send`.

**`hv exec` сдаётся, пока гость ещё грузится.** По умолчанию он ждёт 10 с. Под TCG дайте первой команде больше времени, например `hv exec 0 secs=60 id`.

**`hv on` выдаёт ошибку, или `hv info` не показывает nested paging.** Запускайте QEMU с `-cpu max` (TCG) или с `-enable-kvm -cpu host` (KVM с включённой вложенной виртуализацией).

**`insmod /hv.ko` сообщает `built against another kernel interface`.** `hv.ko` должен быть из той же сборки или того же релиза, что и работающее ядро.

**Гость с `cpus=2` поднимается с одним CPU.** Ядро гостя старше 6.6 или собрано без `SMP`, `X86_X2APIC`, `X86_MPPARSE` либо с недостаточным `NR_CPUS`. Старое ядро всё равно загружается, но на одном CPU.

**Гость с несколькими CPU отвергается.** На свободных ядрах не хватает процессоров. Остановите одного из гостей, включите расширение на большем числе CPU или используйте `hv cores shared`.

**`ping` в госте замолкает после первого ответа.** Ядро гостя собрано без `POSIX_TIMERS`.

**Гость не может достучаться до 10.0.2.2 или другого частного адреса.** NAT не пускает гостей в частные сети и в собственную подсеть nos. Разрешите нужную сеть командой `nat allow <net>`.

**Гость при перезагрузке останавливается.** Без `restart` сброс останавливает VM, а `hv list` показывает причину. Запустите гостя с `restart` или выполните `hv restart <id>`.

**`hv off` отказывается.** На этом CPU ещё работает запущенный гость. Сначала остановите его через `hv stop` или выполните `rmmod hv`, который остановит всех.

## Что дальше

- Всё это может проделать за вас гейт проекта. `nos/scripts/hv-linux-test.py --bzimage linux/arch/x86/boot/bzImage --initrd guest/initrd` загружает гостя и прогоняет долгоживущие VM, а `--disk`, `--net`, `--cpus N` и `--attach` покрывают остальное. `nos/scripts/hv-distro-test.py --iso alpine-virt-3.24.2-x86_64.iso` проверяет Alpine. Оба скрипта используют `nos.iso` и модули, собранные в дереве ([Гейты: как проверять изменения в nos](/howto/run-the-gates)).
- [Гипервизор](/hypervisor) и справочник: [A Linux guest](/docs/hypervisor#a-linux-guest), [Guests that stay up](/docs/hypervisor#guests-that-stay-up), [More than one CPU](/docs/hypervisor#more-than-one-cpu).
- [Установка nos на выделенный сервер](/howto/install-on-a-server), чтобы запускать гостей на настоящем железе.
