---
title: "Linux-гость под гипервизором nos"
description: "Соберите маленькое ядро Linux и initramfs на BusyBox, загрузите hv.ko в nos и запускайте, подключайте к сети и останавливайте Linux-гостей."
order: 6
updated: 2026-10-22
---

Гипервизор nos — это загружаемый модуль `hv.ko`. В итоге он будет запускать внутри nos в QEMU настоящих Linux-гостей:

- собранное вами ядро, загруженное один раз на заданное время командой `hv boot`;
- долгоживущих гостей (`hv start`): вы вводите команды в их консоль, даёте им диск и сеть и останавливаете их;
- Alpine Linux в том виде, в каком он поставляется.

Понадобится:

- **x86-64.** Linux-гости работают под nos только на x86-64.
- **Расширение виртуализации, видимое nos.** В QEMU есть два варианта:
  - **TCG** с `-cpu max` эмулирует AMD-V с вложенной трансляцией страниц (nested paging) на любом хосте, включая Mac. Это медленно: гость эмулируется дважды, и загрузка до шелла занимает минуты.
  - **KVM** на Linux-хосте с включённой вложенной виртуализацией (`nested=1` у `kvm_amd` или `kvm_intel`). `-enable-kvm -cpu host` отдаёт nos собственный AMD-V или Intel VT-x хоста и работает намного быстрее. В TCG нет VT-x, поэтому бэкенд для Intel работает только под KVM или на настоящем железе.
- **`nos.iso` и `hv.ko` из одной сборки или одного релиза.** Ядро отвергает модуль, собранный под другой интерфейс ядра.
- **Клон репозитория nos** — ради его скриптов.
- **x86-64-машина или контейнер с Linux для сборки гостя** и исходники Linux 6.6 или новее. Гейты проекта используют 6.18.
- **QEMU 9.2 или новее** для гостей с несколькими CPU под TCG.

Команды ниже рассчитаны на один рабочий каталог, в котором лежат клон nos как `nos/` и исходники Linux как `linux/`. Рядом с ними вы создадите `guest/` и `rootdir/`.

Как гипервизор устроен внутри, рассказано на [странице гипервизора](/hypervisor) и в [The hypervisor](/docs/hypervisor).

## 1. Соберите ядро гостя

Гость — это немодифицированный 64-битный `bzImage` Linux. Начните с `tinyconfig` и включите то, что предоставляет машина гипервизора. В `linux/`:

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

Зачем нужны опции:

| Опции | Для чего |
|---|---|
| `64BIT` … `SERIAL_8250_CONSOLE` | 64-битное ядро, которое пишет в эмулируемый последовательный порт 8250 — консоль гостя |
| `BLK_DEV_INITRD` … `DEVTMPFS` | initramfs в gzip, ELF-программы и скрипты, `/proc`, `/sys` и devtmpfs |
| `MULTIUSER` … `FILE_LOCKING` | то, на что рассчитывают апплеты BusyBox. Прежде всего `POSIX_TIMERS`: `ping` из BusyBox отмеряет интервалы через `alarm()` и без таймеров после первого ответа ждёт вечно |
| `SMP` … `NR_CPUS 16` | гости с несколькими CPU (`cpus=`): x2APIC, которому нужен `HYPERVISOR_GUEST` или `IRQ_REMAP`, таблица MP и до 16 CPU |
| `PCI` … `EXT4_FS` | диски: legacy virtio поверх PCI, MSI-X для гостя с несколькими CPU и ext4 |
| `NET` … `VIRTIO_NET` | сеть гостей: TCP/IP, пакетные сокеты, автонастройка через `ip=` и virtio-net |

Это набор опций ядра, которое загружает гейт проекта для Linux-гостей. Чтобы гость слышал кнопку питания, которую нажимает `hv stop`, добавьте перед `olddefconfig` ещё `-e ACPI -e ACPI_BUTTON -e X86_PM_TIMER -e INPUT -e INPUT_EVDEV`. Чтобы он по этой кнопке выключался, в нём должен работать `acpid` из BusyBox с обработчиком, который выключает систему ([ACPI](/docs/hypervisor#acpi)). Без ACPI гость кнопку не слышит, и `hv stop` останавливает его сразу.

## 2. Соберите initramfs на BusyBox

initramfs — это статический BusyBox и скрипт `/init`. Какой именно BusyBox взять, важно по трём причинам:

- **Его редактор строки должен спрашивать у терминала позицию курсора** (`ESC [ 6 n`). Модуль вводит строку в гостя только после того, как услышал этот вопрос в приглашении.
- **Нужен `httpd`** — для сетевого примера ниже.
- **Его `exec` должен запускать скрипт без строки `#!` через шелл**, как требует `udhcpc -s`.

Статический BusyBox 1.36.1 из Ubuntu 24.04 умеет все три. В `busybox-static` из Alpine нет `httpd` и такие скрипты он не запускает, а BusyBox из официального Docker-образа `busybox` позицию курсора не спрашивает.

Возьмите бинарник из Ubuntu, напишите init и упакуйте архив:

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

В самой Ubuntu 24.04 Docker не нужен: установите `busybox-static` и скопируйте `/usr/bin/busybox`. `gen_init_cpio` — собственная утилита ядра для сборки архива; она собирается вместе с `bzImage` и создаёт узел устройства без прав root.

> Не убирайте `dmesg -n 1` из init. Строка ядра, напечатанная после приглашения шелла, скрывает это приглашение от модуля, и тогда каждый `hv exec` заканчивается неудачей.

## 3. Подготовьте корневую файловую систему nos

Соберите nos в клоне; после сборки там будут `nos.iso`, `out/x86_64/modules/hv.ko` и `out/x86_64/modules/sshd.ko` ([Сборка nos](/howto/build)):

```sh
cd nos && ./scripts/build-iso-docker.sh && cd ..
```

Вместо этого можно взять `nos.iso`, `hv-x86_64.ko` и `sshd-x86_64.ko` из одного [релиза](https://github.com/irqlevel/nos/releases). Скопируйте модули как `hv.ko` и `sshd.ko` и используйте этот `nos.iso` в команде QEMU ниже.

Соберите всё, что нужно nos, в `rootdir/` и сделайте из каталога образ ext2:

```sh
mkdir -p rootdir
cp nos/out/x86_64/modules/hv.ko nos/out/x86_64/modules/sshd.ko rootdir/
cp linux/arch/x86/boot/bzImage guest/initrd rootdir/
nos/scripts/mkrootfs.sh root.img 256 rootdir
```

`mkrootfs.sh` даёт файловой системе метку `nos` — именно её `root=auto` с ISO монтирует как `/` на чтение и запись. На Mac скрипт запускает `mkfs.ext2` в контейнере `nos-builder`, который создал `build-iso-docker.sh`. Всё, чем будут пользоваться гости, должно лежать в `rootdir/` в момент создания образа. Разделы про диск и Alpine ниже добавляют файлы: положите их в `rootdir/` и перед следующей загрузкой снова запустите `mkrootfs.sh`. Изнутри nos файл можно и скачать — `wget <url> <path>`, до 20 МиБ.

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

- **`-cpu max`** даёт AMD-V из TCG с вложенной трансляцией страниц. Модель CPU QEMU по умолчанию сообщает о SVM без неё, а гипервизору она нужна.
- **`-nographic`** выводит последовательную консоль nos, а с ней и шелл, в ваш терминал. `Ctrl-A X` завершает QEMU.
- **Два проброса портов:** порт хоста 2222 — на sshd в nos, порт хоста 8080 — на порт 8080 в nos, которым ниже пользуется `hv forward`.

Подробнее о запуске nos в QEMU — в руководстве [Запуск nos в QEMU](/howto/run-in-qemu).

## 5. Загрузите гипервизор

Введите в шелле nos:

```sh
insmod /hv.ko
hv info
hv on
```

`hv info` перечисляет, что предлагает расширение процессора, — по одной возможности на строку. `hv on` включает расширение на всех работающих CPU; `hv on 2` включил бы его на одном. Строка `hv` спрашивает у самих процессоров, на каких из них оно включено:

```text
$ insmod /hv.ko
module: hv loaded at 0xFFFF800031000000
$ hv on
hv: turned on for cpu 0-3
hv: AMD-V (SVM) on for cpu 0-3 of 0-3
```

`hv help` перечисляет все подкоманды. `hv off` снова выключает расширение, но отказывается, пока на указанном CPU работает запущенный гость. `rmmod hv` сначала останавливает всех гостей, а затем выключает расширение на всех CPU.

## 6. Гость на минуту: `hv boot`

```sh
hv boot /bzImage initrd=/initrd secs=60 input=id\n cmdline=console=ttyS0 nolapic
```

bzImage и initrd потоком читаются из файлов nos в память гостя, и гость `secs` секунд работает на vCPU. Текст из `input=` вводится в его шелл, как только шелл появится (`\n` — перевод строки). Затем `hv boot` печатает консоль гостя и то, чем всё закончилось. Под TCG отчёт выглядит так (из документации проекта, с сокращениями; строки, которые печатает ваш init, будут другими):

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
| `mem=MiB` | Память гостя; по умолчанию 256, минимум 64. |
| `secs=N` | Сколько `hv boot` держит гостя запущенным; по умолчанию 60, максимум 600. |
| `initrd=path` | initramfs, файл nos. |
| `input=...` | Текст, который нужно ввести в шелл гостя, как только шелл появится. |
| `cmdline=...` | Командная строка ядра гостя. Она забирает весь остаток строки, поэтому ставьте её последней. По умолчанию — `console=ttyS0 nolapic` для гостя с одним CPU и `console=ttyS0` для нескольких. nos добавляет частоту TSC хоста (`tsc_early_khz=`), если строка её ещё не задаёт и частота известна. |

Пока работает `hv boot`, консоль гостя построчно попадает и в лог ядра nos — строками `hvguest| ...`. Консоль гостя, запущенного через `hv start`, попадает туда, только если передать `log`.

## 7. Долгоживущие гости

`hv start` собирает того же гостя, отдаёт его отдельной задаче vCPU и сразу возвращается. Дальше вы обращаетесь к гостю остальными командами `hv` — из консоли, по SSH или через UDP-шелл. В этой стенограмме команды взяты из прогона гейта проекта под TCG, где гость выходит в шелл примерно через секунду после старта:

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
| `hv list` | Перечисляет запущенных гостей: работает ли гость, останавливается ли или чем закончился, на каких CPU хоста он работает, его память, время работы и адрес, число перезапусков и счётчики выходов, прерываний, остановок (hlt) и «пинков» (kicks). |
| `hv exec <id> [secs=N] <line>` | Вводит строку и печатает ответ до приглашения, напечатанного после этой строки (по умолчанию ждёт 10 с). Строка, введённая, пока гость ещё грузится, дожидается его шелла. |
| `hv send <id> <text>` | Вводит текст сразу, как в терминале: `\n` — перевод строки. Работает и в приглашении входа, у которого `exec` прождал бы впустую. |
| `hv wait <id> [secs=N] [boot=N] <text>` | Ждёт, пока в консоли текущей загрузки появится `text`; с `boot=N` — не раньше N-го перезапуска гостя. |
| `hv console <id> [bytes=N]` | Печатает конец его консоли без управляющих последовательностей. |
| `hv restart <id>` | Заново загружает гостя из его файлов, работающего или остановленного. |
| `hv stop <id\|all> [secs=N]` | Нажимает его кнопку питания, даёт `secs` секунд (по умолчанию 30; `0` — не спрашивать) на то, чтобы выключиться самому, затем останавливает и сообщает, чем всё закончилось. |

Используйте id, который напечатал `hv start`: пока модуль загружен, id не переиспользуются. Гость, который сам себя сбрасывает, останавливается, и причиной указывается сброс — если только он не запущен с `restart`. Тогда его каждый раз загружают снова, но не больше пяти раз в минуту.

### Живая консоль по SSH: `hv attach`

`hv attach` — живая консоль гостя: сначала последний килобайт вывода, затем всё, что гость печатает, и каждая нажатая клавиша сразу уходит в гостя. Нужен сеанс, в котором можно печатать, а такой даёт только SSH. Запустите sshd в nos со своим ключом, в шелле nos:

```sh
insmod /sshd.ko
sshd allow ssh-ed25519 AAAA... you@host
sshd start
```

Затем со своей машины:

```sh
ssh -t -p 2222 root@127.0.0.1 hv attach 0
```

`Ctrl-]` отключает вас от консоли; гость продолжает работать. О входе в nos подробнее — в руководстве [Подключение к работающему nos](/howto/connect).

## 8. Дайте гостю диск

`disk=<path>` даёт гостю диск virtio поверх файла nos. Создайте образ ext4 с файлом внутри, положите его в `rootdir/` и пересоздайте `root.img`:

```sh
mkdir -p content && echo hello > content/hello.txt
mke2fs -q -F -t ext4 -d content rootdir/disk.img 32M
nos/scripts/mkrootfs.sh root.img 256 rootdir
```

На Mac нет `mke2fs`, поэтому выполните эту строку в контейнере `nos-builder`. Сначала скопируйте каталог с содержимым внутрь контейнера, как это делает `scripts/mkrootfs.sh`: `mke2fs -d` читает расширенные атрибуты, которых bind mount в macOS не отдаёт:

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

- **Имена и размер.** До четырёх опций `disk=` дают гостю `vda`, `vdb` и далее по порядку. Каждый диск имеет размер своего файла: гость пишет в его пределах и никогда его не увеличивает.
- **Один пишет или многие читают.** `disk=path:ro` делает диск для гостя доступным только на чтение. Образ либо пишет один гость, либо читают сколько угодно гостей, а запуск, который нарушил бы это правило, отвергается с указанием владельца: `hv: vm 1 not started -- /disk.img: vm 0's disk writes it`.
- **Сброс кэша доходит до диска.** Flush гостя синхронизирует файловую систему nos, на которой лежит образ.

## 9. Подключите гостя к сети

`net` у `hv start` даёт гостю сетевую карту virtio на коммутаторе, общем с другими гостями и с nos. Первый порт получает 10.0.100.2, следующий — 10.0.100.3 и так далее; адрес передаётся ядру гостя через `ip=`. Сам nos — 10.0.100.1. Запустите двух гостей:

```sh
hv start /bzImage initrd=/initrd net cmdline=console=ttyS0 nolapic
hv start /bzImage initrd=/initrd net cmdline=console=ttyS0 nolapic
hv exec 0 secs=60 ping -c 3 10.0.100.1
hv exec 0 ping -c 3 10.0.100.3
ping 10.0.100.2
```

Первые два ping выполняются в госте 0 — до nos и до второго гостя. Последний — собственный `ping` nos через его виртуальную сетевую карту `hv0`.

**Доступ к гостю снаружи даёт `hv forward`.** Он ретранслирует порт nos на порт гостя. Поднимите страницу в госте 1 и пробросьте на неё порт 8080 nos:

```sh
hv exec 1 mkdir -p /www
hv exec 1 echo hello from a guest > /www/index.html
hv exec 1 httpd -p 80 -h /www
hv forward add 8080 1 80
```

nos ответит `hv: port 8080 forwarded to vm 1, 10.0.100.3:80`. QEMU пробрасывает порт хоста 8080 в nos, так что со своей машины:

```sh
curl http://127.0.0.1:8080/index.html
```

**Во внешний мир гости выходят через NAT nos**, с собственного адреса nos. На DHCP-запрос гостя коммутатор отвечает сам: адрес порта, nos как маршрутизатор и DNS-сервер nos. В initramfs на BusyBox нет `/etc/resolv.conf`, поэтому, чтобы имена разрешались, направьте его на DNS-сервер, который ядру передал `ip=`:

```sh
hv exec 0 mkdir -p /etc && ln -sf /proc/net/pnp /etc/resolv.conf
```

NAT не пускает гостей в частные сети, на link-local-адреса (там отвечает сервер метаданных облака), в общее пространство адресов carrier-grade NAT и в собственную подсеть аплинка nos. В user-сети QEMU под последнее попадает и хост по адресу 10.0.2.2: `nat allow 10.0.2.2/32` пускает к нему гостей, а `nat` показывает, что делает NAT. Подробнее — в [A network](/docs/hypervisor#a-network).

## 10. Несколько CPU

`cpus=N` даёт гостю N процессоров, до 16, и каждый — отдельная задача vCPU на своём CPU хоста. Ядру гостя нужен Linux 6.6 или новее с `SMP`, `X86_X2APIC` и `X86_MPPARSE` — в рецепте выше всё это есть. Командная строка по умолчанию теряет `nolapic`, а `noapic` nos добавляет сам, потому что IO-APIC у машины нет, если не передать `ioapic`. Из прогона на одном из серверов проекта, с сокращениями:

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

- **Каждому vCPU нужен свой CPU хоста.** Два процессора одного гостя никогда не делят один CPU хоста, поэтому у гостя может быть не больше процессоров, чем охватывает `hv on`.
- **Гость получает ядра целиком.** По умолчанию (`hv cores own`) ядра гостя принадлежат только ему, и других гостей на них нет; гость, которому понадобилось бы ядро, занятое другим гостем, отвергается с объяснением. `hv cores shared` разрешает гостям делить ядра — для машины, где все гости ваши, а `hv cores` показывает, кто какие CPU занимает.
- **Под TCG используйте QEMU 9.2 или новее.** До 9.2 TCG не пропускает обращения гостя с выключенной страничной адресацией через вложенную таблицу страниц. Второй CPU гостя стартует в реальном режиме, поэтому он работал бы прямо в памяти nos и портил бы её. KVM и настоящее железо всегда делали это правильно.

## 11. Дистрибутив: Alpine

ISO Alpine в варианте `virt` загружается как есть. Ядро и initramfs извлекаются из ISO, а сам ISO становится диском гостя, доступным только на чтение. Скачайте `alpine-virt-<version>-x86_64.iso` с сайта Alpine, затем извлеките и разложите файлы (`xorriso` есть в большинстве дистрибутивов):

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

Его initramfs загружает `virtio_blk`, находит ISO на `vda`, ставит базовую систему из пакетов ISO в tmpfs, а OpenRC доводит загрузку до приглашения входа на `ttyS0`. getty позицию курсора не спрашивает, поэтому входите через `hv send` — она вводит текст сразу. После первого `send` строки `exec` тоже уходят сразу. На коммутаторе initramfs Alpine настраивает `eth0` по `ip=` и записывает DNS-сервер nos в `resolv.conf`, так что `apk` добирается до своих зеркал через NAT. `reboot` сбрасывает гостя, а `restart` загружает его снова; чтобы дождаться приглашения следующей загрузки, используйте `hv wait 0 boot=1 login:`. Загружается и облачный образ Debian: см. [A distribution](/docs/hypervisor#a-distribution).

## На сервере

На сервере, установленном из системного образа ([Установка nos на выделенный сервер](/howto/install-on-a-server)), модуль уже лежит в `/lib/modules/hv.ko`, поэтому шаг 5 начинается с `insmod /lib/modules/hv.ko`. Файлы гостя положите на раздел nos из Linux сервера до загрузки nos или скачайте их изнутри nos через `wget`. Серверы проекта запускают гостей под AMD-V процессора Ryzen 5 3600 и под VT-x процессора i5-13500 ([On real hardware](/docs/hypervisor#on-real-hardware)).

## Если что-то пошло не так

**`hv: vm 0: the guest has not read the line -- it is not at a prompt; the line stays queued`.** Модуль так и не увидел, что шелл гостя спросил позицию курсора. Проверьте, что ваш BusyBox её спрашивает (шаг 2) и что в init есть `dmesg -n 1`. В приглашении входа пользуйтесь `hv send`.

**`hv exec` сдаётся, пока гость ещё грузится.** По умолчанию он ждёт 10 с. Под TCG дайте первой команде больше времени, например `hv exec 0 secs=60 id`.

**`hv on` не срабатывает, или `hv info` не показывает nested paging.** Запускайте QEMU с `-cpu max` (TCG) или с `-enable-kvm -cpu host` (KVM с включённой вложенной виртуализацией).

**`insmod /hv.ko` сообщает `built against another kernel interface`.** `hv.ko` должен быть из той же сборки или того же релиза, что и работающее ядро.

**Гость с `cpus=2` поднимается с одним CPU.** Ядро гостя старше 6.6 или в нём нет `SMP`, `X86_X2APIC`, `X86_MPPARSE` либо достаточного `NR_CPUS`. Старое ядро всё равно загружается на одном CPU.

**Гость с несколькими CPU отвергается.** На свободных ядрах не хватает процессоров. Остановите одного из гостей, включите расширение на большем числе CPU или используйте `hv cores shared`.

**`ping` в госте замолкает после первого ответа.** В ядре гостя нет `POSIX_TIMERS`.

**Гость не может достучаться до 10.0.2.2 или другого частного адреса.** NAT не пускает гостей в частные сети и в собственную подсеть nos. Разрешите нужное командой `nat allow <net>`.

**Гость, который перезагружается, вместо этого останавливается.** Без `restart` сброс останавливает VM, а `hv list` показывает причину. Запустите гостя с `restart` или выполните `hv restart <id>`.

**`hv off` отказывается.** На этом CPU ещё работает запущенный гость: сначала остановите его через `hv stop` или выполните `rmmod hv`, который остановит всех.

## Что дальше

- Пусть всё это сделает гейт проекта: `nos/scripts/hv-linux-test.py --bzimage linux/arch/x86/boot/bzImage --initrd guest/initrd` загружает гостя и прогоняет долгоживущие VM, а `--disk`, `--net`, `--cpus N` и `--attach` покрывают остальное. `nos/scripts/hv-distro-test.py --iso alpine-virt-3.24.2-x86_64.iso` проверяет Alpine. Оба используют `nos.iso` и модули, собранные в дереве ([Гейты: как проверять изменения в nos](/howto/run-the-gates)).
- [Гипервизор](/hypervisor) и справочник: [A Linux guest](/docs/hypervisor#a-linux-guest), [Guests that stay up](/docs/hypervisor#guests-that-stay-up), [More than one CPU](/docs/hypervisor#more-than-one-cpu).
- [Установка nos на выделенный сервер](/howto/install-on-a-server) — чтобы запускать гостей на настоящем железе.
