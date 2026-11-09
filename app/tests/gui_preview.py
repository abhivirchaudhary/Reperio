#!/usr/bin/env python3
"""Drives the real app to check the viewer and "Group similar photos".

Needs what gui_e2e.py needs, and the card from make_preview_card.py
attached as a loop device:

    losetup -f --show preview.img
    python3 app/tests/gui_preview.py SHOTS_DIR
"""
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

shots = sys.argv[1]
app = os.path.abspath("target/debug/reperio-app")
os.makedirs(shots, exist_ok=True)

driver_proc = subprocess.Popen(["tauri-driver"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
time.sleep(1.5)
opts = webdriver.common.options.ArgOptions()
opts.set_capability("browserName", "wry")
opts.set_capability("tauri:options", {"application": app})
d = webdriver.Remote("http://127.0.0.1:4444", options=opts)
wait = WebDriverWait(d, 90)


def shot(name):
    path = os.path.join(shots, name + ".png")
    if os.environ.get("DISPLAY") and shutil.which("import"):
        time.sleep(0.5)
        subprocess.run(["import", "-window", "root", path], check=False)
    else:
        d.save_screenshot(path)
    print("screenshot", path)


def card(name):
    return d.find_element(By.XPATH, f"//div[contains(@class,'card')][.//div[@class='name'][text()='{name}']]")


def open_card(name):
    el = card(name)
    d.execute_script("arguments[0].scrollIntoView({block: 'center'})", el)
    time.sleep(0.3)
    ActionChains(d).double_click(el).perform()


def close_viewer():
    ActionChains(d).send_keys(Keys.ESCAPE).perform()
    wait.until(lambda d: not d.find_elements(By.CSS_SELECTOR, ".viewer"))


try:
    d.set_window_size(1280, 820)
    wait.until(EC.element_to_be_clickable((By.XPATH, "//button[contains(@class,'disk')][.//div[text()='Disk image']]"))).click()
    wait.until(EC.presence_of_element_located((By.CSS_SELECTOR, ".results")))
    wait.until(lambda d: not d.find_elements(By.CSS_SELECTOR, ".scanning-chip:not([hidden])"))
    wait.until(lambda d: len(d.find_elements(By.CSS_SELECTOR, ".card .thumb img.loaded")) >= 9)
    names = [e.get_attribute("textContent") for e in d.find_elements(By.CSS_SELECTOR, ".card .name")]
    print("cards:", len(names), names)
    shot("1-grid")

    # The video plays.
    open_card("MVI_0121.WEBM")
    try:
        wait.until(EC.presence_of_element_located((By.CSS_SELECTOR, ".viewer video")))
    except Exception:
        shot("2-video-failed")
        print("viewer says:", d.find_element(By.CSS_SELECTOR, ".viewer .media").text)
        raise
    try:
        wait.until(lambda d: d.execute_script("const v=document.querySelector('.viewer video'); return v && v.currentTime > 1.5"))
    except Exception:
        shot("2-video-failed")
        print(d.execute_script("const v=document.querySelector('.viewer video'); return v ? [v.error && v.error.message, v.readyState, v.networkState, v.src] : document.querySelector('.viewer .media').textContent"))
        raise
    state = d.execute_script("const v=document.querySelector('.viewer video'); return [v.videoWidth, v.videoHeight, v.duration, v.paused]")
    print("video playing:", state)
    assert state[0] == 640 and state[1] == 360 and not state[3], state
    shot("2-video")
    # Arrows move on, and stop the sound.
    ActionChains(d).send_keys(Keys.ARROW_RIGHT).perform()
    time.sleep(0.5)
    close_viewer()

    # An MP4 whose codec this machine lacks says so kindly.
    open_card("MVI_0120.MP4")
    time.sleep(3)
    print("mp4:", d.find_element(By.CSS_SELECTOR, ".viewer .media").text or "plays")
    close_viewer()

    # A photo, fitted, then at 100%, then moved.
    open_card("IMG_0101.JPG")
    wait.until(lambda d: d.execute_script("const i=document.querySelector('.viewer .zoombox img'); return i && i.complete && i.naturalWidth > 0"))
    time.sleep(0.6)
    print("fitted:", d.find_element(By.CSS_SELECTOR, ".zlabel").text)
    shot("3-photo-fit")
    ActionChains(d).send_keys("1").perform()
    time.sleep(0.4)
    box = d.find_element(By.CSS_SELECTOR, ".viewer .zoombox")
    ActionChains(d).move_to_element(box).click_and_hold().move_by_offset(-200, -120).release().perform()
    time.sleep(0.4)
    zoom = d.find_element(By.CSS_SELECTOR, ".zlabel").text
    print("after 1 and drag:", zoom, d.execute_script("return document.querySelector('.viewer .zoombox img').style.transform"))
    assert zoom == "100%", zoom
    shot("4-photo-100")
    # Space selects; the panel says so.
    ActionChains(d).send_keys(" ").perform()
    time.sleep(0.3)
    print("select button:", d.find_element(By.CSS_SELECTOR, ".viewer .select-btn").text)
    ActionChains(d).send_keys(" ").perform()
    close_viewer()

    # Audio, text, and a PDF's text.
    for name, sel, label in [("Voice memo.mp3", ".viewer audio", "5-audio"), ("_otes.txt", ".viewer pre", "6-text"), ("Plans.pdf", ".viewer pre", "7-pdf")]:
        open_card(name)
        wait.until(EC.presence_of_element_located((By.CSS_SELECTOR, sel)))
        time.sleep(0.8)
        if sel.endswith("pre"):
            print(name, "->", d.find_element(By.CSS_SELECTOR, sel).text.replace("\n", " | ")[:80])
        shot(label)
        close_viewer()

    # Group similar photos.
    before = len(d.find_elements(By.CSS_SELECTOR, ".card"))
    d.find_element(By.CSS_SELECTOR, "button.switch[title='Group similar photos']").click()
    wait.until(lambda d: d.find_elements(By.CSS_SELECTOR, ".card.stacked"))
    time.sleep(1.5)
    stacks = {e.find_element(By.CSS_SELECTOR, ".name").get_attribute("textContent"): e.find_element(By.CSS_SELECTOR, ".stack-n").text
              for e in d.find_elements(By.CSS_SELECTOR, ".card.stacked")}
    after = len(d.find_elements(By.CSS_SELECTOR, ".card"))
    print(f"grouped: {before} cards -> {after}; stacks: {stacks}")
    shot("8-grouped")
    best = next(n for n, c in stacks.items() if c == "+3")
    open_card(best)
    wait.until(EC.presence_of_element_located((By.CSS_SELECTOR, ".group-sheet .gtile")))
    time.sleep(1.2)
    members = [e.get_attribute("textContent") for e in d.find_elements(By.CSS_SELECTOR, ".group-sheet .gtile .name")]
    print("group members:", members)
    shot("9-group-sheet")
    # Choose only the best, then all again.
    d.find_element(By.XPATH, "//div[contains(@class,'group-sheet')]//button[text()='Only the best']").click()
    print(d.find_element(By.CSS_SELECTOR, ".gsum").text)
    d.find_element(By.XPATH, "//div[contains(@class,'group-sheet')]//button[text()='Done']").click()
    time.sleep(0.6)
    print("card state:", card(best).get_attribute("aria-checked"), "·", d.find_element(By.CSS_SELECTOR, ".bottombar .summary").text)
    shot("10-partial")
finally:
    d.quit()
    driver_proc.terminate()
