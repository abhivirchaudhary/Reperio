#!/usr/bin/env python3
"""The "Run a plug-in" screen: a tool plug-in with arguments, its printed output shown.

    DISPLAY=:99 python3 app/tests/gui_tools.py SHOTS_DIR
"""
import os
import shutil
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
os.makedirs(f"{work}/plugins")
shutil.copy("scripts/free_tool.py", f"{work}/plugins/")
dest = f"{work}/dest"
os.makedirs(dest)
os.environ.update(REPERIO_PLUGINS=f"{work}/plugins", REPERIO_TEST_DEST=dest)
failures = []


def check(name, ok):
    print(("ok    " if ok else "FAIL  ") + name, flush=True)
    if not ok:
        failures.append(name)


proc = subprocess.Popen(["tauri-driver"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
time.sleep(1.5)
o = webdriver.common.options.ArgOptions()
o.set_capability("browserName", "wry")
o.set_capability("tauri:options", {"application": os.path.abspath("target/debug/reperio-app")})
d = webdriver.Remote("http://127.0.0.1:4444", options=o)
try:
    w = WebDriverWait(d, 60)
    time.sleep(1.5)
    d.execute_script("localStorage.removeItem('reperio.settings')")
    d.refresh()
    w.until(EC.element_to_be_clickable((By.XPATH, "//button[contains(.,'Run a plug-in')]"))).click()
    w.until(lambda d: "free-tool" in d.find_element(By.CSS_SELECTOR, ".sheet.tools").text)
    time.sleep(0.5)
    subprocess.run(["import", "-window", "root", f"{shots}/tools-1.png"])
    check("lists the tool plug-in", True)
    box = d.find_element(By.CSS_SELECTOR, ".sheet.tools input[type=text]")
    box.send_keys('one "two words" --flag')
    d.execute_script("[...document.querySelectorAll('.sheet.tools .btn.primary')].pop().click()")
    w.until(lambda d: "What the plug-in printed" in d.find_element(By.CSS_SELECTOR, ".sheet.tools").text)
    time.sleep(0.6)
    subprocess.run(["import", "-window", "root", f"{shots}/tools-2.png"])
    text = d.find_element(By.CSS_SELECTOR, ".sheet.tools").text
    check("what the tool printed is shown, with its arguments", "tool started with args: ['one', 'two words', '--flag']" in text)
    check("its file is listed", "result.txt" in text)
    saved = [r for r, _, f in os.walk(dest) if "result.txt" in f]
    check("the file and logs were saved", bool(saved) and os.path.exists(f"{saved[0]}/console.log") and os.path.exists(f"{saved[0]}/audit.log"))
finally:
    d.quit()
    proc.terminate()

sys.exit(1 if failures else 0)
