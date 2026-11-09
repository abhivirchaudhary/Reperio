#!/usr/bin/env python3
"""Drives the real Reperio app through a full recovery with WebDriver.

Needs: tauri-driver, WebKitWebDriver, selenium, a display (Xvfb), a debug
build of reperio-app, and a test card attached as a loop device:

    losetup -f --show CARDS/fat32.img
    REPERIO_TEST_DEST=/tmp/out python3 app/tests/gui_e2e.py CARDS fat32.img SHOTS_DIR
"""
import glob
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time

from selenium import webdriver
from selenium.webdriver.common.action_chains import ActionChains
from selenium.webdriver.common.by import By
from selenium.webdriver.common.keys import Keys
from selenium.webdriver.support import expected_conditions as EC
from selenium.webdriver.support.ui import WebDriverWait

cards, card, shots = sys.argv[1], sys.argv[2], sys.argv[3]
dest = os.environ["REPERIO_TEST_DEST"]
app = os.path.abspath("target/debug/reperio-app")
os.makedirs(shots, exist_ok=True)
shutil.rmtree(dest, ignore_errors=True)
os.makedirs(dest)

driver_proc = subprocess.Popen(["tauri-driver"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
time.sleep(1.5)
opts = webdriver.common.options.ArgOptions()
opts.set_capability("browserName", "wry")
opts.set_capability("tauri:options", {"application": app})
d = webdriver.Remote("http://127.0.0.1:4444", options=opts)
wait = WebDriverWait(d, 60)

def shot(name):
    # WebKit's WebDriver snapshots skip scrollable layers such as dialogs,
    # so capture the real screen when there is one.
    path = os.path.join(shots, name + ".png")
    if os.environ.get("DISPLAY") and shutil.which("import"):
        time.sleep(0.4)
        subprocess.run(["import", "-window", "root", path], check=False)
    else:
        d.save_screenshot(path)

try:
    d.set_window_size(1280, 820)
    disk = wait.until(EC.element_to_be_clickable((By.XPATH, "//button[contains(@class,'disk')][.//div[text()='Disk image']]")))
    shot("1-home")
    disk.click()

    # Scan screen, then results once the scan finishes.
    # A small card scans in under a second; the progress screen may be gone already.
    wait.until(lambda d: d.find_elements(By.ID, "pct") or d.find_elements(By.CSS_SELECTOR, ".results"))
    if d.find_elements(By.ID, "pct"):
        shot("2-scanning")
    wait.until(EC.presence_of_element_located((By.CSS_SELECTOR, ".results")))
    wait.until(lambda d: not d.find_elements(By.CSS_SELECTOR, ".scanning-chip:not([hidden])"))
    wait.until(lambda d: len(d.find_elements(By.CSS_SELECTOR, ".card .thumb img")) >= 8)
    time.sleep(2)  # thumbnails decode
    shot("3-results")
    names = [e.get_attribute("textContent") for e in d.find_elements(By.CSS_SELECTOR, ".card .name")]
    print("cards:", names)
    assert "Birthday party - Mum & me.jpg" in names and "IMG_0001.JPG" in names, names
    assert "notes.txt" not in names, "files still on the card are hidden by default"
    summary = d.find_element(By.CSS_SELECTOR, ".bottombar .summary").text
    print("selection:", summary)

    # Preview a photo.
    first = d.find_element(By.XPATH, "//div[contains(@class,'card')][.//div[text()='IMG_0001.JPG']]")
    d.execute_script("arguments[0].scrollIntoView({block: 'center'})", first)
    time.sleep(0.3)
    ActionChains(d).double_click(first).perform()
    wait.until(EC.presence_of_element_located((By.CSS_SELECTOR, ".viewer .stage img")))
    wait.until(lambda d: d.execute_script("const i=document.querySelector('.viewer .stage img'); return i && i.complete && i.naturalWidth > 0"))
    shot("4-preview")
    ActionChains(d).send_keys(Keys.ESCAPE).perform()
    wait.until(lambda d: not d.find_elements(By.CSS_SELECTOR, ".viewer"))

    # List view.
    d.find_element(By.ID, "mode-list").click()
    time.sleep(0.5)
    shot("5-list")
    d.find_element(By.ID, "mode-grid").click()

    # Recover everything selected.
    d.find_element(By.CSS_SELECTOR, ".bottombar .btn.primary").click()
    wait.until(EC.presence_of_element_located((By.CSS_SELECTOR, ".sheet")))
    d.find_element(By.XPATH, "//div[contains(@class,'sheet')]//button[text()='Choose…']").click()
    wait.until(EC.presence_of_element_located((By.CSS_SELECTOR, ".dest .path.set")))
    shot("6-recover-sheet")
    d.find_element(By.XPATH, "//div[contains(@class,'sheet')]//button[text()='Recover']").click()
    wait.until(EC.presence_of_element_located((By.CSS_SELECTOR, ".done-icon")))
    shot("7-done")
    print("done:", d.find_element(By.CSS_SELECTOR, ".sheet h2").get_attribute("textContent"))
finally:
    d.quit()
    driver_proc.terminate()

# Every deleted file must be back at its original path, byte for byte.
manifest = json.load(open(os.path.join(cards, "manifest.json")))[card]
sha = lambda p: hashlib.sha256(open(p, "rb").read()).hexdigest()
got = {sha(p): os.path.relpath(p, dest) for p in glob.glob(f"{dest}/**/*", recursive=True) if os.path.isfile(p)}
missing = [e["name"] for e in manifest if e["deleted"] and got.get(e["sha256"]) != os.path.join(e["dir"], e["name"])]
assert not missing, f"not recovered intact: {missing}\n{got}"
print(f"GUI recovery OK: {sum(e['deleted'] for e in manifest)} deleted files recovered byte for byte")
