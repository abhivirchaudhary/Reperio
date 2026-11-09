/* Example Reperio plug-in in C (standard C library only).
 *
 * Reads a made-up file system called TOYFS so the example can run; replace
 * scan() with your own parsing. Build and install:
 *
 *     cc -O2 -o toyfs-c toyfs.c
 *     cp toyfs-c <a plug-ins folder>        (reperio plugins lists them)
 *
 * TOYFS layout:
 *   0..8   "TOYFS001"        8..12  entry count (u32 LE)
 *   then 64-byte entries: name[40], start u64, size u64, flags u32 (1 = deleted)
 *
 * Protocol (full description: docs/plugins.md): Reperio sends one JSON line
 * on stdin; you answer with JSON lines on stdout. To read the drive, print
 * {"read":{...}} and read one reply line back. Never open the device yourself.
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define NAME "toyfs-c"
#define WANTS "[\"unknown\"]" /* ntfs, fat, exfat, hfs+, apfs, ext, unknown, any */

static char line[1 << 20];

static int b64val(int c) {
    if (c >= 'A' && c <= 'Z') return c - 'A';
    if (c >= 'a' && c <= 'z') return c - 'a' + 26;
    if (c >= '0' && c <= '9') return c - '0' + 52;
    if (c == '+') return 62;
    if (c == '/') return 63;
    return -1;
}

/* Decodes base64 from `s` up to the closing quote. Returns bytes written. */
static size_t b64decode(const char *s, uint8_t *out) {
    size_t n = 0;
    uint32_t acc = 0;
    int bits = 0, v;
    for (; *s && *s != '"'; s++) {
        if ((v = b64val((unsigned char)*s)) < 0) continue;
        acc = acc << 6 | (uint32_t)v;
        bits += 6;
        if (bits >= 8) {
            bits -= 8;
            out[n++] = (uint8_t)(acc >> bits);
            acc &= (1u << bits) - 1;
        }
    }
    return n;
}

/* Reads `len` bytes at `offset` of the volume through Reperio (offset 0 is
 * the volume's first byte). Returns the number of bytes, 0 on failure. */
static size_t read_volume(uint64_t offset, uint64_t len, uint8_t *out) {
    printf("{\"read\":{\"offset\":%llu,\"len\":%llu}}\n", (unsigned long long)offset,
           (unsigned long long)len);
    fflush(stdout);
    if (!fgets(line, sizeof line, stdin)) return 0;
    char *data = strstr(line, "\"data\":\"");
    return data ? b64decode(data + 8, out) : 0;
}

static void scan(void) {
    static uint8_t head[12];
    if (read_volume(0, 12, head) < 12 || memcmp(head, "TOYFS001", 8) != 0)
        return; /* not ours: say nothing, Reperio moves on */
    uint32_t count;
    memcpy(&count, head + 8, 4); /* little endian hosts */
    if (count > 100000) return;
    uint8_t *table = malloc(64 * (size_t)count + 1);
    if (!table) return;
    size_t got = read_volume(12, 64 * (uint64_t)count, table);
    for (size_t i = 0; i + 64 <= got; i += 64) {
        char name[41] = {0};
        uint64_t start, size;
        uint32_t flags;
        memcpy(name, table + i, 40);
        memcpy(&start, table + i + 40, 8);
        memcpy(&size, table + i + 48, 8);
        memcpy(&flags, table + i + 56, 4);
        printf("{\"file\":{\"path\":\"Photos/%s\",\"size\":%llu,\"deleted\":%s,"
               "\"extents\":[[%llu,%llu]]}}\n",
               name, (unsigned long long)size, (flags & 1) ? "true" : "false",
               (unsigned long long)start, (unsigned long long)size);
    }
    puts("{\"done\":{\"label\":\"TOYFS volume\"}}");
    fflush(stdout);
    free(table);
}

int main(void) {
    if (!fgets(line, sizeof line, stdin)) return 0;
    if (strstr(line, "\"info\"")) {
        puts("{\"info\":{\"name\":\"" NAME "\",\"fs\":" WANTS "}}");
        fflush(stdout);
    } else if (strstr(line, "\"scan\"")) {
        scan();
    }
    return 0;
}
