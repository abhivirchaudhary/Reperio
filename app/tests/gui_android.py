#!/usr/bin/env python3
"""Drives the app's Android screen against a fake phone (scripts/mock_adb.py).

Needs: tauri-driver, WebKitWebDriver, selenium, Xvfb on :99, a debug build.

    DISPLAY=:99 python3 app/tests/gui_android.py SHOTS_DIR
"""
import os
import subprocess
import sys
import shutil
import tempfile
import time

from selenium import webdriver
from selenium.webdriver.common.by import By
from selenium.webdriver.support import expected_conditions as EC
from selenium.webdriver.support.ui import WebDriverWait

shots = sys.argv[1]
os.makedirs(shots, exist_ok=True)
work = tempfile.mkdtemp()
phone = f"{work}/phone"
for path, text in [
    ("sdcard/Pictures/.trashed-1893456000-IMG_0001.jpg", "photo bytes"),
    ("sdcard/DCIM/Camera/.trashed-1893456001-IMG_0002.jpg", "other photo"),
    ("sdcard/DCIM/.thumbnails/1234", "thumb"),
    ("sdcard/DCIM/Camera/kept.jpg", "alive"),
]:
    os.makedirs(os.path.dirname(f"{phone}/{path}"), exist_ok=True)
    open(f"{phone}/{path}", "w").write(text)
open(f"{work}/userdata.img", "wb").write(os.urandom(2_000_000))
os.makedirs(f"{phone}/parts")
open(f"{phone}/parts/disk", "wb").write(os.urandom(2_500_000))
open(f"{phone}/parts/boot", "wb").write(os.urandom(70_000))
dest = f"{work}/dest"
os.makedirs(dest)

os.environ.update(ADB=os.path.abspath("scripts/mock_adb.py"), MOCK_PHONE=phone, MOCK_IMG=f"{work}/userdata.img",
                  REPERIO_TEST_DEST=dest)
failures = []


def check(name, ok):
    print(("ok    " if ok else "FAIL  ") + name)
    if not ok:
        failures.append(name)


def session(crypto, root):
    os.environ.update(MOCK_CRYPTO=crypto, MOCK_ROOT=root)
    proc = subprocess.Popen(["tauri-driver"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(1.5)
    o = webdriver.common.options.ArgOptions()
    o.set_capability("browserName", "wry")
    o.set_capability("tauri:options", {"application": os.path.abspath("target/debug/reperio-app")})
    d = webdriver.Remote("http://127.0.0.1:4444", options=o)
    return proc, d


def shot(name):
    subprocess.run(["import", "-window", "root", f"{shots}/{name}.png"])


def button(d, text):
    return d.execute_script("return [...document.querySelectorAll('.android button')].find(b=>b.textContent.trim().startsWith(arguments[0]))", text)


# 1. An encrypted phone without root: copy what it kept.
proc, d = session("encrypted", "0")
try:
    w = WebDriverWait(d, 40)
    w.until(EC.element_to_be_clickable((By.XPATH, "//button[contains(.,'Android phone')]"))).click()
    w.until(lambda d: d.find_elements(By.CSS_SELECTOR, ".android .phone"))
    time.sleep(0.6)
    shot("android-1-phone")
    text = d.find_element(By.CSS_SELECTOR, ".sheet.android").text
    check("shows the phone", "Mock Pixel Test" in text and "Android 14" in text)
    check("says it is encrypted and not rooted", "Encrypted" in text and "Not rooted" in text)
    check("image button is off without root", not button(d, "Choose a folder and image").is_enabled())
    button(d, "Choose a folder and copy").click()
    w.until(lambda d: "put back" in d.find_element(By.CSS_SELECTOR, ".sheet.android").text)
    time.sleep(0.5)
    shot("android-2-pulled")
    out = [r for r, _, f in os.walk(dest) if "Recovered from trash" in r]
    names = sorted(os.listdir(out[0])) if out else []
    check("trashed files put back", names == ["IMG_0001.jpg", "IMG_0002.jpg"])
    check("result names them", "IMG_0001.jpg" in d.find_element(By.CSS_SELECTOR, ".sheet.android").text)
finally:
    d.quit()
    proc.terminate()

# 2. A rooted phone that is not encrypted: image it, then scan the image.
proc, d = session("unencrypted", "1")
try:
    w = WebDriverWait(d, 40)
    w.until(EC.element_to_be_clickable((By.XPATH, "//button[contains(.,'Android phone')]"))).click()
    w.until(lambda d: d.find_elements(By.CSS_SELECTOR, ".android .phone"))
    time.sleep(0.6)
    shot("android-3-rooted")
    check("image button is on", button(d, "Choose a folder and image").is_enabled())
    button(d, "Choose a folder and image").click()
    w.until(lambda d: "imaged" in d.find_element(By.CSS_SELECTOR, ".sheet.android").text)
    time.sleep(0.5)
    shot("android-4-imaged")
    img = [os.path.join(dest, f) for f in os.listdir(dest) if f.endswith("-full-storage.img")]
    check("whole storage chip imaged, exact", img and open(img[0], "rb").read() == open(f"{phone}/parts/disk", "rb").read())
    check("hash report written", os.path.exists(img[0] + ".hashes.txt"))
    button(d, "Scan the image").click()
    w.until(lambda d: d.find_elements(By.CSS_SELECTOR, ".scan, .results"))
    check("scan of the image starts", True)
finally:
    d.quit()
    proc.terminate()

# 3. Every partition, one file each, with a manifest.
shutil.rmtree(dest); os.makedirs(dest)
proc, d = session("unencrypted", "1")
try:
    w = WebDriverWait(d, 40)
    w.until(EC.element_to_be_clickable((By.XPATH, "//button[contains(.,'Android phone')]"))).click()
    w.until(lambda d: d.find_elements(By.CSS_SELECTOR, ".android .phone"))
    time.sleep(1.5)
    shot("android-5-choices")
    text = d.find_element(By.CSS_SELECTOR, ".sheet.android").text
    check("offers whole chip, every partition and user data", all(t in text for t in ["The whole storage chip", "Every partition", "User data only"]))
    d.execute_script("[...document.querySelectorAll('.android .choice')].find(c=>c.textContent.includes('Every partition')).click()")
    time.sleep(0.4)
    button(d, "Choose a folder and image").click()
    w.until(lambda d: "partitions imaged" in d.find_element(By.CSS_SELECTOR, ".sheet.android").text)
    time.sleep(0.5)
    shot("android-6-all")
    folder = [os.path.join(dest, f) for f in os.listdir(dest) if f.endswith("-all-partitions")]
    files = sorted(os.listdir(folder[0])) if folder else []
    check("one file per partition and a manifest", files == ["MANIFEST.txt", "boot.img", "userdata.img"])
    check("partition image exact", open(f"{folder[0]}/boot.img", "rb").read() == open(f"{phone}/parts/boot", "rb").read())
finally:
    d.quit()
    proc.terminate()

# 4. Forensic plug-ins: none installed, then one loaded.
os.makedirs(f"{work}/plugins")
os.environ["REPERIO_PLUGINS"] = f"{work}/plugins"
proc, d = session("encrypted", "0")
try:
    w = WebDriverWait(d, 40)
    w.until(EC.element_to_be_clickable((By.XPATH, "//button[contains(.,'Android phone')]"))).click()
    w.until(lambda d: d.find_elements(By.CSS_SELECTOR, ".android .phone"))
    w.until(lambda d: "No forensic plug-ins installed yet" in d.find_element(By.CSS_SELECTOR, ".sheet.android").text)
    check("says no forensic plug-ins are installed", True)
finally:
    d.quit()
    proc.terminate()

shutil.copy("plugins/examples/analyzer_device_report.py", f"{work}/plugins/")
shutil.rmtree(dest); os.makedirs(dest)
proc, d = session("encrypted", "0")
try:
    w = WebDriverWait(d, 40)
    w.until(EC.element_to_be_clickable((By.XPATH, "//button[contains(.,'Android phone')]"))).click()
    w.until(lambda d: d.find_elements(By.CSS_SELECTOR, ".android .phone"))
    w.until(lambda d: "device-report-python" in d.find_element(By.CSS_SELECTOR, ".sheet.android").text)
    time.sleep(0.5)
    shot("android-7-plugins")
    check("lists the installed forensic plug-in", True)
    button(d, "Choose a folder and run").click()
    w.until(lambda d: "audit log" in d.find_element(By.CSS_SELECTOR, ".sheet.android").text.lower())
    time.sleep(0.6)
    shot("android-8-analysis")
    text = d.find_element(By.CSS_SELECTOR, ".sheet.android").text
    check("shows the findings", "Files in Android's trash" in text and "WhatsApp is installed" in text)
    folders = [os.path.join(r, "device-report-python") for r, ds, _ in os.walk(dest) if "device-report-python" in ds]
    ok = folders and all(os.path.exists(f"{folders[0]}/{n}") for n in ["report.json", "audit.log", "MANIFEST.sha256.txt", "trashed-media.csv"])
    check("report, audit log and files were saved", ok)
finally:
    d.quit()
    proc.terminate()

sys.exit(1 if failures else 0)
