#include "grub.h"

#include <kernel/trace.h>
#include <kernel/parameters.h>
#include <mm/memory_map.h>
#include <lib/stdlib.h>

namespace kernel
{

namespace grub
{

/* The multiboot info lives in usable RAM that is later handed to the
   page allocator, so the RSDP is copied out during parsing. RSDP v2 is
   36 bytes; keep some slack. */
static u8 acpi_rsdp_copy[64];
static size_t acpi_rsdp_size;
static bool acpi_rsdp_is_new;

/* Regions the kernel adds to the memory map itself once the firmware's are
   in: PageArray's, with room to spare */
static const ulong KERNEL_CARVE_OUTS = 4;

/* The `ucode` module's physical range; UcodeEnd 0 for none */
static const char UCODE_MODULE_NAME[] = "ucode";
static ulong ucode_start;
static ulong ucode_end;

static bool framebuffer_present;
static u8 framebuffer_type;
static FramebufferInfo framebuffer;

bool get_ucode_module(ulong& phys, ulong& len)
{
    if (ucode_end == 0)
        return false;
    phys = ucode_start;
    len = ucode_end - ucode_start;
    return true;
}

const void* get_acpi_rsdp(size_t& size)
{
    size = acpi_rsdp_size;
    return (acpi_rsdp_size != 0) ? acpi_rsdp_copy : nullptr;
}

bool has_framebuffer_info()
{
    return framebuffer_present;
}

bool is_framebuffer_ega_text()
{
    return framebuffer_type == MULTI_BOOT_FRAMEBUFFER_TYPE_EGA_TEXT;
}

const FramebufferInfo* get_framebuffer_info()
{
    return framebuffer_present ? &framebuffer : nullptr;
}

static void save_acpi_rsdp(MultiBootTag* tag, bool is_new)
{
    /* Prefer the v2 (new) RSDP if both tags are present */
    if (acpi_rsdp_size != 0 && acpi_rsdp_is_new && !is_new)
        return;

    if (tag->size <= sizeof(MultiBootTag))
        return;

    size_t rsdp_size = tag->size - sizeof(MultiBootTag);
    if (rsdp_size > sizeof(acpi_rsdp_copy))
        rsdp_size = sizeof(acpi_rsdp_copy);

    MultiBootTagAcpi* acpi_tag = reinterpret_cast<MultiBootTagAcpi*>(tag);
    stdlib::mem_cpy(acpi_rsdp_copy, acpi_tag->rsdp, rsdp_size);
    acpi_rsdp_size = rsdp_size;
    acpi_rsdp_is_new = is_new;
}

void parse_multi_boot_info(MultiBootInfoHeader *mb_info)
{
    trace(0, "MbInfo %p", mb_info);

    const void* mb_info_end = stdlib::mem_add(mb_info, mb_info->total_size);
    MultiBootTag * tag;
    /* A tag's header is read only once it is known to be inside the info: an
       info with no end tag, or one shorter than its header, ends here */
    for (tag = reinterpret_cast<MultiBootTag*>(mb_info + 1);
        stdlib::mem_add(tag, sizeof(*tag)) <= mb_info_end &&
        tag->type != MULTI_BOOT_TAG_TYPE_END;
        )
    {
        /* A malformed tag must not walk past the info buffer or spin forever */
        if (stdlib::mem_add(tag, tag->size) > mb_info_end ||
            tag->size < sizeof(*tag))
        {
            trace(0, "Malformed tag %lu size %lu, stop parsing", (ulong)tag->type, (ulong)tag->size);
            break;
        }

        trace(0, "Tag %lu Size %lu", (ulong)tag->type, (ulong)tag->size);
        switch (tag->type)
        {
        case MULTI_BOOT_TAG_TYPE_BOOT_DEV:
        {
            if (tag->size < sizeof(MultiBootTagBootDev))
                break;

            MultiBootTagBootDev* bdev = reinterpret_cast<MultiBootTagBootDev*>(tag);
            trace(0, "Boot dev 0x%lX 0x%lX 0x%lX",
                (ulong)bdev->bios_dev, (ulong)bdev->slice, (ulong)bdev->part);
            break;
        }
        case MULTI_BOOT_TAG_TYPE_MMAP:
        {
            if (tag->size < sizeof(MultiBootTagMmap))
                break;

            MultiBootTagMmap* mmap = reinterpret_cast<MultiBootTagMmap*>(tag);
            MultiBootMmapEntry* entry;

            if (mmap->entry_size < sizeof(MultiBootMmapEntry))
                break;

            /* The map is a table of fixed size, and this runs before any
               console a headless machine has: a firmware's map too long for
               it must not stop the boot without a word. So reserved regions
               go in first and usable RAM after, leaving room for the
               kernel's own carve-outs (PageArray's): a reserved region left
               out would be pages handed out that are not RAM, a usable one
               only RAM not used, which is said. Only reserved regions too
               many for the table by themselves stop the boot. */
            auto& memory_map = kernel::mm::MemoryMap::get_instance();
            ulong dropped = 0;
            ulong dropped_bytes = 0;
            for (ulong pass = 0; pass < 2; pass++)
            {
                const bool usable_pass = (pass == 1);
                for (entry = &mmap->entry[0];
                     stdlib::mem_add(entry, mmap->entry_size) <= stdlib::mem_add(mmap, mmap->size);
                     entry = reinterpret_cast<MultiBootMmapEntry*>(stdlib::mem_add(entry, mmap->entry_size)))
                {
                    const bool usable = (entry->type == MULTI_BOOT_MEMORY_AVAILABLE);
                    if (usable != usable_pass)
                        continue;

                    trace(0, "Mmap addr 0x%lX len 0x%lX type %lu",
                        (ulong)entry->addr, (ulong)entry->len, (ulong)entry->type);

                    if (usable && memory_map.get_region_count() + KERNEL_CARVE_OUTS >= kernel::mm::MemoryMap::MAX_REGIONS)
                    {
                        dropped++;
                        dropped_bytes += entry->len;
                        continue;
                    }

                    if (!memory_map.add_region((ulong)entry->addr, (ulong)entry->len, (ulong)entry->type))
                        PANIC("Can't add memory region");
                }
            }

            if (dropped != 0)
                trace(0, "mm: %lu usable regions, %lu MiB, past what the memory map holds, not used",
                    dropped, dropped_bytes / consts::MB);
            break;
        }
        case MULTI_BOOT_TAG_TYPE_CMDLINE:
        {
            if (tag->size <= sizeof(MultiBootTagString))
                break;

            MultiBootTagString* cmd_line = reinterpret_cast<MultiBootTagString*>(tag);

            /* The string must be NUL-terminated within the tag */
            size_t max_len = tag->size - sizeof(MultiBootTagString);
            bool terminated = false;
            for (size_t i = 0; i < max_len; i++)
            {
                if (cmd_line->string[i] == '\0')
                {
                    terminated = true;
                    break;
                }
            }
            if (!terminated)
            {
                trace(0, "Cmdline tag is not NUL-terminated, ignored");
                break;
            }

            trace(0, "Cmdline %s", cmd_line->string);
            kernel::Parameters::get_instance().parse(cmd_line->string);

            break;
        }
        case MULTI_BOOT_TAG_TYPE_MODULE:
        {
            if (tag->size <= sizeof(MultiBootTagModule))
                break;

            MultiBootTagModule* module = reinterpret_cast<MultiBootTagModule*>(tag);

            /* Its command line must be NUL-terminated within the tag */
            size_t max_len = tag->size - sizeof(MultiBootTagModule);
            size_t name_len = 0;
            while (name_len < max_len && module->cmd_line[name_len] != '\0')
                name_len++;
            if (name_len == max_len)
            {
                trace(0, "Module tag's command line is not NUL-terminated, ignored");
                break;
            }
            if (stdlib::str_cmp(module->cmd_line, UCODE_MODULE_NAME) != 0)
            {
                trace(0, "Module %s ignored", module->cmd_line);
                break;
            }
            if (module->mod_end <= module->mod_start)
            {
                trace(0, "Module ucode is empty, ignored");
                break;
            }

            ucode_start = module->mod_start;
            ucode_end = module->mod_end;
            trace(0, "Module ucode 0x%lX-0x%lX", ucode_start, ucode_end);
            break;
        }
        case MULTI_BOOT_TAG_TYPE_ACPI_OLD:
        {
            save_acpi_rsdp(tag, false);
            trace(0, "Acpi old RSDP tag, size %lu", (ulong)tag->size);
            break;
        }
        case MULTI_BOOT_TAG_TYPE_ACPI_NEW:
        {
            save_acpi_rsdp(tag, true);
            trace(0, "Acpi new RSDP tag, size %lu", (ulong)tag->size);
            break;
        }
        case MULTI_BOOT_TAG_TYPE_FRAMEBUFFER:
        {
            if (tag->size < MULTI_BOOT_TAG_FRAMEBUFFER_COMMON_SIZE)
                break;

            MultiBootTagFramebuffer* fb = reinterpret_cast<MultiBootTagFramebuffer*>(tag);
            framebuffer_present = true;
            framebuffer_type = fb->fb_type;

            framebuffer.addr = fb->addr;
            framebuffer.pitch = fb->pitch;
            framebuffer.width = fb->width;
            framebuffer.height = fb->height;
            framebuffer.bpp = fb->bpp;
            framebuffer.type = fb->fb_type;

            /* Color field layout only exists in the RGB variant */
            if (fb->fb_type == MULTI_BOOT_FRAMEBUFFER_TYPE_RGB &&
                tag->size >= sizeof(MultiBootTagFramebuffer))
            {
                framebuffer.red_pos = fb->red_field_position;
                framebuffer.red_size = fb->red_mask_size;
                framebuffer.green_pos = fb->green_field_position;
                framebuffer.green_size = fb->green_mask_size;
                framebuffer.blue_pos = fb->blue_field_position;
                framebuffer.blue_size = fb->blue_mask_size;
            }
            else if (fb->fb_type == MULTI_BOOT_FRAMEBUFFER_TYPE_RGB)
            {
                /* Truncated tag: assume the usual little-endian xRGB8888 */
                framebuffer.red_pos = 16;
                framebuffer.red_size = 8;
                framebuffer.green_pos = 8;
                framebuffer.green_size = 8;
                framebuffer.blue_pos = 0;
                framebuffer.blue_size = 8;
            }

            trace(0, "Framebuffer addr 0x%lX %lux%lu pitch %lu bpp %lu type %lu",
                (ulong)fb->addr, (ulong)fb->width, (ulong)fb->height,
                (ulong)fb->pitch, (ulong)fb->bpp, (ulong)fb->fb_type);
            break;
        }
        default:
        {
            break;
        }
        }

        /* Size >= sizeof(*tag) was checked above, so this always advances */
        tag = reinterpret_cast<MultiBootTag*>(stdlib::mem_add(tag, (tag->size + 7) & ~7));
    }
}

}

}