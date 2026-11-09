#!/usr/bin/env python3
# A fake `adb` for testing `reperio android` and the app's Android screen
# without a phone. Set ADB to this file and MOCK_PHONE (a folder holding
# sdcard/), MOCK_IMG (the userdata partition), MOCK_CRYPTO (encrypted or
# unencrypted) and MOCK_ROOT (1 or 0).
#!/usr/bin/env python3
import os, shutil, sys, subprocess
a = sys.argv[1:]
phone, img = os.environ["MOCK_PHONE"], os.environ["MOCK_IMG"]


def parts():
    """The phone's partitions: userdata is MOCK_IMG, the others are small files in MOCK_PHONE/parts."""
    out = {"userdata": img}
    d = os.path.join(phone, "parts")
    for n in sorted(os.listdir(d)) if os.path.isdir(d) else []:
        if n != "disk":
            out[n] = os.path.join(d, n)
    return out


def disk():
    return os.path.join(phone, "parts", "disk")


def device_file(script):
    """The file behind the /dev/block path named in a script."""
    import re
    m = re.search(r"/dev/block/by-name/([\w.-]+)", script)
    if m:
        return parts()[m.group(1)]
    m = re.search(r"/dev/block/(sd\w+)", script)
    return disk() if m and m.group(1) == "sda" else img
if a[:1] == ["devices"]:
    print("List of devices attached\nMOCK123\tdevice"); sys.exit(0)
assert a[:2] == ["-s", "MOCK123"], a
a = a[2:]
if a[0] == "shell":
    s = a[1]
    if s.startswith("getprop "):
        p = s.split()[1]
        print({"ro.product.manufacturer": "Mock", "ro.product.model": "Pixel Test", "ro.build.version.release": "14",
               "ro.build.version.security_patch": "2024-05-01", "ro.crypto.state": os.environ["MOCK_CRYPTO"],
               "ro.crypto.type": "file"}.get(p, ""))
    elif "su -c id" in s:
        if os.environ["MOCK_ROOT"] == "1": print("uid=0(root) gid=0(root)")
        else: sys.exit(1)
    elif s.startswith("find "):
        for d, _, files in os.walk(phone + "/sdcard"):
            for f in files:
                p = os.path.join(d, f)
                if f.startswith(".trashed-") or "/.thumbnails/" in p:
                    print(p[len(phone):])
    elif s.startswith("pm list packages"):
        print("package:com.whatsapp\npackage:com.android.chrome")
    elif s.startswith("df"):
        print("Filesystem  Size  Used  Avail  Use%  Mounted on\n/dev/fuse  100G  40G  60G  40%  /storage/emulated")
    elif s.startswith("content query"):
        for d, _, files in os.walk(phone + "/sdcard"):
            for f in sorted(files):
                if f.startswith(".trashed-"):
                    exp = f.split("-")[1]
                    print(f"Row: 0 _data={os.path.join(d, f)[len(phone):]}, date_expires={exp}")
    elif s.startswith("ls ") and "sdcard" in s:
        p = phone + s.split()[-1]
        print("\n".join(sorted(os.listdir(p))) if os.path.isdir(p) else "")
    elif s.startswith("touch ") or s.startswith("it's ") or s == "id":
        open(phone + "/commands.log", "a").write(s + "\n")
        print("uid=0(root)" if os.environ["MOCK_ROOT"] == "1" else "uid=2000(shell)")
    elif "for p in /dev/block/by-name" in s:
        for n, p in parts().items():
            print(n, os.path.getsize(p))
    elif "for d in /sys/block" in s:
        print("sda", os.path.getsize(disk()))
        print("sdb", 4096)
    elif "blockdev" in s:
        print(os.path.getsize(device_file(s)))
elif a[0] == "get-state":
    print("device")
elif a[0] == "pull":
    src, dst = a[-2], a[-1]
    src = phone + src
    if src.endswith("/."):
        shutil.copytree(src[:-2], dst, dirs_exist_ok=True)
    else:
        shutil.copy2(src, dst)
elif a[0] == "exec-out":
    data = open(device_file(a[1]), "rb").read()
    data += bytes((-len(data)) % 1048576)  # dd with sync pads to a whole block
    sys.stdout.buffer.write(data)
