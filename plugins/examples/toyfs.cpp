// Example Reperio plug-in in C++17 (standard library only).
//
// Reads a made-up file system called TOYFS so the example can run; replace
// scan() with your own parsing. Build and install:
//
//     c++ -std=c++17 -O2 -o toyfs-cpp toyfs.cpp
//     cp toyfs-cpp <a plug-ins folder>        (reperio plugins lists them)
//
// TOYFS layout:
//   0..8   "TOYFS001"        8..12  entry count (u32 LE)
//   then 64-byte entries: name[40], start u64, size u64, flags u32 (1 = deleted)
//
// Protocol (full description: docs/plugins.md): Reperio sends one JSON line
// on stdin; you answer with JSON lines on stdout. To read the drive, print
// {"read":{...}} and read one reply line back. Never open the device yourself.

#include <cstdint>
#include <cstring>
#include <iostream>
#include <string>
#include <vector>

namespace {

constexpr const char *NAME = "toyfs-cpp";
constexpr const char *WANTS = R"(["unknown"])";  // ntfs, fat, exfat, hfs+, apfs, ext, unknown, any

std::vector<uint8_t> base64_decode(const std::string &s) {
    auto val = [](char c) -> int {
        if (c >= 'A' && c <= 'Z') return c - 'A';
        if (c >= 'a' && c <= 'z') return c - 'a' + 26;
        if (c >= '0' && c <= '9') return c - '0' + 52;
        return c == '+' ? 62 : c == '/' ? 63 : -1;
    };
    std::vector<uint8_t> out;
    uint32_t acc = 0;
    int bits = 0;
    for (char c : s) {
        int v = val(c);
        if (v < 0) continue;
        acc = acc << 6 | static_cast<uint32_t>(v);
        bits += 6;
        if (bits >= 8) {
            bits -= 8;
            out.push_back(static_cast<uint8_t>(acc >> bits));
            acc &= (1u << bits) - 1;
        }
    }
    return out;
}

// Bytes of the volume (offset 0 is its first byte), read through Reperio.
std::vector<uint8_t> read_volume(uint64_t offset, uint64_t len) {
    std::cout << R"({"read":{"offset":)" << offset << R"(,"len":)" << len << "}}" << std::endl;
    std::string reply;
    if (!std::getline(std::cin, reply)) return {};
    auto at = reply.find("\"data\":\"");
    if (at == std::string::npos) return {};  // {"ok":false}
    at += 8;
    return base64_decode(reply.substr(at, reply.find('"', at) - at));
}

void scan() {
    auto head = read_volume(0, 12);
    if (head.size() < 12 || std::memcmp(head.data(), "TOYFS001", 8) != 0)
        return;  // not ours: say nothing, Reperio moves on
    uint32_t count;
    std::memcpy(&count, head.data() + 8, 4);  // little endian hosts
    if (count > 100000) return;
    auto table = read_volume(12, 64ull * count);
    for (size_t i = 0; i + 64 <= table.size(); i += 64) {
        const uint8_t *e = table.data() + i;
        std::string name(reinterpret_cast<const char *>(e), strnlen(reinterpret_cast<const char *>(e), 40));
        uint64_t start, size;
        uint32_t flags;
        std::memcpy(&start, e + 40, 8);
        std::memcpy(&size, e + 48, 8);
        std::memcpy(&flags, e + 56, 4);
        std::cout << R"({"file":{"path":"Photos/)" << name << R"(","size":)" << size
                  << R"(,"deleted":)" << ((flags & 1) ? "true" : "false")
                  << R"(,"extents":[[)" << start << "," << size << "]]}}\n";
    }
    std::cout << R"({"done":{"label":"TOYFS volume"}})" << std::endl;
}

}  // namespace

int main() {
    std::string line;
    if (!std::getline(std::cin, line)) return 0;
    if (line.find("\"info\"") != std::string::npos)
        std::cout << R"({"info":{"name":")" << NAME << R"(","fs":)" << WANTS << "}}" << std::endl;
    else if (line.find("\"scan\"") != std::string::npos)
        scan();
    return 0;
}
