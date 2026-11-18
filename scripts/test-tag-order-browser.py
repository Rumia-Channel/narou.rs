"""Exercise the upgrade tour and tag order against both Native backends."""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
from urllib.error import URLError
from urllib.request import ProxyHandler, Request, build_opener

from selenium.common.exceptions import StaleElementReferenceException
from selenium import webdriver
from selenium.webdriver.common.by import By
from selenium.webdriver.support.ui import Select, WebDriverWait

ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / "target/debug" / ("narou_rs.exe" if os.name == "nt" else "narou_rs")
FIXTURE_TAGS = {8: ["Zulu", "Middle", "Alpha", "2"], 1448: ["Middle", "Zulu", "Beta", "2"]}


def run_mode(driver, root, mode):
    library, home = root / "library", root / "home"
    state = library / ".narou"
    state.mkdir(parents=True)
    settings = home / ".narousetting"
    settings.mkdir(parents=True)
    (settings / "global_setting.yaml").write_text("server-ws-port: 0\n", encoding="utf-8")
    # This represents an existing library with first-run prompts already handled.
    (state / "last-run-version").write_text("0.4.0\n", encoding="utf-8")
    (state / "local_setting.yaml").write_text(
        "webui.tag-sort-order: favorites_first\nwebui.feature-tour.seen-version: '0.6.6'\n", encoding="utf-8",
    )
    (state / "favorite_tags.yaml").write_text("- Zulu\n- Middle\n", encoding="utf-8")
    (state / "tag_colors.yaml").write_text("Zulu: blue\nMiddle: magenta\n", encoding="utf-8")
    if mode == "sqlite":
        (state / "storage-backend").write_text("sqlite", encoding="utf-8")
    with (state / "database.yaml").open("w", encoding="utf-8") as database:
        for id_, tags in FIXTURE_TAGS.items():
            record = {"id": id_, "title": f"Fixture {id_}", "author": "Fixture author",
                      "file_title": f"fixture-{id_}", "toc_url": f"https://example.invalid/{id_}/",
                      "sitename": "Fixture", "novel_type": 1, "end": False, "tags": tags,
                      "last_update": "2026-11-01 00:00:00.000000000 +00:00", "ncode": f"fixture{id_}",
                      "use_subdirectory": False, "suspend": False, "is_narou": False}
            database.write(f"{id_}: {json.dumps(record)}\n")
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    base = f"http://127.0.0.1:{port}"
    http = build_opener(ProxyHandler({}))
    env = {key: os.environ[key] for key in ("PATH", "PATHEXT", "SYSTEMROOT", "WINDIR", "SYSTEMDRIVE", "TEMP", "TMP")
           if key in os.environ}
    env.update(HOME=str(home), USERPROFILE=str(home), NAROU_RS_LEGACY_YAML="0" if mode == "sqlite" else "1")
    with (root / "server.log").open("wb") as log:
        server = subprocess.Popen([BINARY, "web", "--no-browser", "--port", str(port)], cwd=library, env=env,
                                  stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 25
            while True:
                assert server.poll() is None and time.monotonic() < deadline, f"{mode}: Native Web startup failed"
                try:
                    with http.open(base + "/", timeout=1):
                        if (state / "server.pid").is_file():
                            break
                except URLError:
                    pass
                time.sleep(0.05)
            control = (state / "server.pid").read_text(encoding="utf-8").split()[1]

            def api(path):
                request = Request(base + path, headers={"x-narou-internal-token": control})
                with http.open(request, timeout=5) as response:
                    return json.load(response)

            assert api("/api/storage/mode")["mode"] == mode
            token = (state / "ui_token").read_text(encoding="utf-8").strip()
            driver.get(base + "/assets/js/main.js")
            driver.delete_all_cookies()
            driver.add_cookie({"name": "narou_ui_token", "value": token, "path": "/", "httpOnly": True, "sameSite": "Strict"})
            driver.get(base + "/")
            exercise_upgrade_tour(driver)
            exercise_tags(driver, api)
            print(f"Firefox + Native {mode}: four orders, tag/favorite edits, stored order and colors verified", flush=True)
        finally:
            server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=5)


def exercise_upgrade_tour(driver):
    modal = (By.ID, "feature-tour-modal")
    wait = WebDriverWait(driver, 12)
    wait.until(lambda _: driver.find_element(*modal).is_displayed())
    tour = driver.find_element(*modal).text
    assert "v0.6.7" in tour and "ノベルアップ＋に対応" in tour and "PierreL-H" in tour, tour
    driver.find_element(By.ID, "feature-tour-ok").click()
    wait.until_not(lambda _: driver.find_element(*modal).is_displayed())


def exercise_tags(driver, api):
    wait = WebDriverWait(driver, 12, poll_frequency=0.1, ignored_exceptions=(StaleElementReferenceException,))

    def el(css):
        return driver.find_element(By.CSS_SELECTOR, css)

    def click(css):
        wait.until(lambda _: el(css).is_displayed() and el(css).is_enabled())
        el(css).click()

    def row_tags(id_):
        return driver.find_elements(By.CSS_SELECTOR, f'tr[data-id="{id_}"] .col-tags .tag-label')

    def expect(a, b):
        expected = [a.split(), b.split()]

        def visible(_):
            actual = [[badge.get_dom_attribute("data-tag") for badge in row_tags(id_)] for id_ in FIXTURE_TAGS]
            return actual == expected

        wait.until(visible, message=f"Expected tag column order {expected}")

    expect("Zulu Middle 2 Alpha", "Zulu Middle 2 Beta")
    colors = api("/api/tag_list?format=json")["tag_colors"]
    menu = '[data-dropdown="tag-menu"]'
    order = "#tag-sort-order-select"
    for value, a, b in [
        ("alphabetical", "2 Alpha Middle Zulu", "2 Beta Middle Zulu"),
        ("count", "2 Middle Zulu Alpha", "2 Middle Zulu Beta"),
        ("registered", "2 Alpha Middle Zulu", "2 Middle Zulu Beta"),
        ("favorites_first", "Zulu Middle 2 Alpha", "Zulu Middle 2 Beta"),
    ]:
        if not el(order).is_displayed():
            click(menu)
        Select(el(order)).select_by_value(value)
        expect(a, b)
        assert api("/api/webui/config")["tag_sort_order"] == value
        assert api("/api/tag_list?format=json")["tag_colors"] == colors

    if el(order).is_displayed():
        click(menu)
    click('tr[data-id="1448"] .col-title')
    click(menu)
    click("#action-tag-edit")
    el("#new-tag-input").send_keys("Added")
    click("#add-tag-button")
    wait.until(lambda _: not el("#new-tag-input").get_property("value"))
    expect("Zulu Middle 2 Alpha", "Zulu Middle 2 Added Beta")
    el("#new-tag-input").send_keys("Added")
    click("#favorite-tag-button")
    expect("Zulu Middle 2 Alpha", "Zulu Middle Added 2 Beta")
    assert api("/api/favorite_tags")["tags"] == ["Zulu", "Middle", "Added"]
    saved = {record["id"]: record["tags"] for record in api("/api/list?all=true")["data"]}
    assert saved == {8: FIXTURE_TAGS[8], 1448: FIXTURE_TAGS[1448] + ["Added"]}
    assert all(api("/api/tag_list?format=json")["tag_colors"][tag] == color for tag, color in colors.items())


def main():
    os.environ.setdefault("SE_CACHE_PATH", str(ROOT / "target/transfer-browser-cache"))
    options = webdriver.FirefoxOptions()
    options.add_argument("-headless")
    if binary := os.environ.get("FIREFOX_BINARY"):
        options.binary_location = binary
    scratch = ROOT / "target/test-libraries"
    scratch.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="tag-order-browser-", dir=scratch) as directory:
        root = Path(directory)
        service = webdriver.FirefoxService(service_args=["--profile-root", directory])
        with webdriver.Firefox(options=options, service=service) as driver:
            driver.set_window_size(1440, 1000)
            for mode in ("yaml", "sqlite"):
                run_mode(driver, root / mode, mode)


if __name__ == "__main__":
    main()
