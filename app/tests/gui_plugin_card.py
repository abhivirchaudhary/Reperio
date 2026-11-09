#!/usr/bin/env python3
"""The card that asks which plug-ins to use before a scan.

A rooted fake phone (scripts/mock_adb.py) whose user-data image holds a TOYFS
volume, with the example reader (toyfs.py) and the example analyzer installed.
Run 1 keeps both ticked: the reader finds beach.jpg and the analyzer's report
appears. Run 2 unticks both: neither does.

    DISPLAY=:99 python3 app/tests/gui_plugin_card.py SHOTS_DIR
"""
import os
import shutil
import struct
import subprocess
import sys
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
os.makedirs(f"{phone}/sdcard")
os.makedirs(f"{phone}/parts")
open(f"{phone}/parts/disk", "wb").write(os.urandom(1000))

files = [("beach.jpg", 30000, 6000, 1), ("alive.txt", 40000, 500, 0)]
img = bytearray(os.urandom(200_000))
table = b"TOYFS001" + struct.pack("<I", len(files))
for name, start, size, flags in files:
    table += struct.pack("<40sQQI", name.encode(), start, size, flags) + bytes(12)
img[: len(table)] = table
img[30000:36000] = b"\xff\xd8\xff\xe0" + bytes([9]) * 5996
open(f"{work}/userdata.img", "wb").write(img)

os.makedirs(f"{work}/plugins")
shutil.copy("plugins/examples/toyfs.rs", f"{work}/plugins/")  # source code: Reperio builds it
shutil.copy("plugins/examples/analyzer_device_report.py", f"{work}/plugins/")
dest = f"{work}/dest"
os.makedirs(dest)
os.environ.update(ADB=os.path.abspath("scripts/mock_adb.py"), MOCK_PHONE=phone, MOCK_IMG=f"{work}/userdata.img",
                  MOCK_CRYPTO="unencrypted", MOCK_ROOT="1", REPERIO_TEST_DEST=dest,
                  REPERIO_PLUGINS=f"{work}/plugins")
failures = []


def check(name, ok):
    print(("ok    " if ok else "FAIL  ") + name, flush=True)
    if not ok:
        failures.append(name)


def shot(name):
    subprocess.run(["import", "-window", "root", f"{shots}/{name}.png"])


def fresh(d):
    """The app remembers settings between runs; start each run from none."""
    time.sleep(1.5)
    d.execute_script("localStorage.removeItem('reperio.settings')")
    d.refresh()
    time.sleep(1.5)


def run(tick):
    shutil.rmtree(os.path.expanduser("~/.local/share/app.reperio.recovery/scans"), ignore_errors=True)
    shutil.rmtree(dest); os.makedirs(dest)
    proc = subprocess.Popen(["tauri-driver"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(1.5)
    o = webdriver.common.options.ArgOptions()
    o.set_capability("browserName", "wry")
    o.set_capability("tauri:options", {"application": os.path.abspath("target/debug/reperio-app")})
    d = webdriver.Remote("http://127.0.0.1:4444", options=o)
    try:
        w = WebDriverWait(d, 60)
        fresh(d)
        w.until(EC.element_to_be_clickable((By.XPATH, "//button[contains(.,'Android phone')]"))).click()
        w.until(lambda d: d.find_elements(By.CSS_SELECTOR, ".android .phone"))
        time.sleep(1.0)
        d.execute_script("[...document.querySelectorAll('.android .choice')].find(c=>c.textContent.includes('User data only')).click()")
        time.sleep(0.4)
        d.execute_script("[...document.querySelectorAll('.android button')].find(b=>b.textContent.trim()==='Choose a folder and image').click()")
        w.until(lambda d: "imaged" in d.find_element(By.CSS_SELECTOR, ".sheet.android").text)
        d.execute_script("[...document.querySelectorAll('.android button')].find(b=>b.textContent.trim()==='Scan the image').click()")
        w.until(lambda d: d.find_elements(By.CSS_SELECTOR, ".plugin-card .pitem"))
        time.sleep(0.6)
        card = d.find_element(By.CSS_SELECTOR, ".plugin-card")
        if tick:
            shot("plugin-card")
            check("the card lists both kinds of plug-in", all(t in card.text for t in ["toyfs-rust", "device-report-python", "Reader", "Analyzer"]))
            check("both are ticked to begin with", all(c.is_selected() for c in card.find_elements(By.CSS_SELECTOR, "input[type=checkbox]")[:2]))
        else:
            # Unticking redraws the card, so find the boxes again each time.
            for _ in range(4):
                d.execute_script("const b=[...document.querySelectorAll('.plugin-card .pitem input')].find(x=>x.checked); if(b) b.click()")
                time.sleep(0.2)
            card = d.find_element(By.CSS_SELECTOR, ".plugin-card")
            check("can untick them (button says scan without plug-ins)", "Scan without plug-ins" in card.text)
        d.execute_script("[...document.querySelectorAll('.plugin-card .btn.primary')][0].click()")
        # The scan runs; wait for results (the quick scan, then the deep scan if nothing was found).
        w2 = WebDriverWait(d, 150)
        try:
            w2.until(lambda d: d.find_elements(By.CSS_SELECTOR, ".results"))
        except Exception:
            shot("plugin-timeout")
            print("PAGE:", d.find_element(By.CSS_SELECTOR, "body").text[:600].replace("\n", " | "), flush=True)
            print("CLASSES:", d.execute_script("return [...document.querySelectorAll('#app *')].slice(0,40).map(e=>e.className).filter(Boolean).join(' ; ')"), flush=True)
            raise
        time.sleep(2.0)
        page = d.find_element(By.CSS_SELECTOR, "body").text
        shot("plugin-results-ticked" if tick else "plugin-results-unticked")
        found = "1 deleted file found" in page
        check("the reader found the deleted file (beach.jpg)" if tick else "no reader, no deleted file found", found == tick)
        if tick:
            w3 = WebDriverWait(d, 40)
            w3.until(lambda d: d.find_elements(By.CSS_SELECTOR, "#reports-slot button"))
            check("the analyzer's report is offered", True)
            d.find_element(By.CSS_SELECTOR, "#reports-slot button").click()
            time.sleep(0.8)
            shot("plugin-report")
            check("the report shows its findings", "Image size" in d.find_element(By.CSS_SELECTOR, ".overlay").text)
        else:
            check("no report without an analyzer", not d.find_elements(By.CSS_SELECTOR, "#reports-slot button"))
    finally:
        d.quit()
        proc.terminate()


def run_broken():
    """Only a file that cannot be used is in the folder: the card says why."""
    os.makedirs(f"{work}/broken", exist_ok=True)
    open(f"{work}/broken/oops.rs", "w").write("fn main() { this is broken }\n")
    os.environ["REPERIO_PLUGINS"] = f"{work}/broken"
    shutil.rmtree(os.path.expanduser("~/.local/share/app.reperio.recovery/scans"), ignore_errors=True)
    shutil.rmtree(dest); os.makedirs(dest)
    proc = subprocess.Popen(["tauri-driver"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(1.5)
    o = webdriver.common.options.ArgOptions()
    o.set_capability("browserName", "wry")
    o.set_capability("tauri:options", {"application": os.path.abspath("target/debug/reperio-app")})
    d = webdriver.Remote("http://127.0.0.1:4444", options=o)
    try:
        w = WebDriverWait(d, 60)
        fresh(d)
        # Settings -> Plug-ins says why, too.
        w.until(EC.element_to_be_clickable((By.CSS_SELECTOR, "button[title=Settings]"))).click()
        w.until(lambda d: d.find_elements(By.CSS_SELECTOR, ".settings-nav button"))
        d.execute_script("[...document.querySelectorAll('.settings-nav button')].find(b=>b.textContent.trim()==='Plug-ins').click()")
        w.until(lambda d: "oops.rs" in d.find_element(By.CSS_SELECTOR, ".settings-body").text)
        shot("plugin-settings-problem")
        body = d.find_element(By.CSS_SELECTOR, ".settings-body").text
        check("Settings says oops.rs could not be used and why", "did not build" in body)
        d.execute_script("[...document.querySelectorAll('.overlay .btn.primary')].pop().click()")
        time.sleep(0.6)
        # And before a scan.
        w.until(EC.element_to_be_clickable((By.XPATH, "//button[contains(.,'Android phone')]"))).click()
        w.until(lambda d: d.find_elements(By.CSS_SELECTOR, ".android .phone"))
        time.sleep(1.0)
        d.execute_script("[...document.querySelectorAll('.android .choice')].find(c=>c.textContent.includes('User data only')).click()")
        time.sleep(0.4)
        d.execute_script("[...document.querySelectorAll('.android button')].find(b=>b.textContent.trim()==='Choose a folder and image').click()")
        w.until(lambda d: "imaged" in d.find_element(By.CSS_SELECTOR, ".sheet.android").text)
        d.execute_script("[...document.querySelectorAll('.android button')].find(b=>b.textContent.trim()==='Scan the image').click()")
        w.until(lambda d: d.find_elements(By.CSS_SELECTOR, ".plugin-card"))
        time.sleep(0.6)
        shot("plugin-card-problem")
        card = d.find_element(By.CSS_SELECTOR, ".plugin-card").text
        check("the card before the scan explains why no plug-in is offered", "No plug-in could be used" in card and "oops.rs" in card and "did not build" in card)
    finally:
        d.quit()
        proc.terminate()


mode = os.environ.get("ONLY")
if mode == "broken":
    run_broken()
else:
    if mode != "untick":
        run(True)
    run(False)
    run_broken()
sys.exit(1 if failures else 0)
