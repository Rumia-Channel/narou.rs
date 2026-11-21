"""Actual shipped UI/WASM through 10 MiB gateways; no transfer API stand-ins.

Install selenium==4.52.0, build transfer assets, then run with Python. Firefox
and geckodriver are supplied by Selenium Manager or FIREFOX_BINARY/PATH.
The larger >4 GiB manual acceptance is separate from this bounded CI smoke.
"""
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import shutil
import subprocess
import tempfile
import time
import zipfile

from selenium import webdriver
from selenium.webdriver.common.by import By
from selenium.webdriver.support.ui import WebDriverWait

ROOT = Path(__file__).resolve().parent.parent
ARTIFACTS = ROOT / "target/transfer-browser-artifacts"


def wait_json(path, process, timeout=240):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"Fixture process exited {process.returncode} before readiness")
        if path.exists():
            try:
                return json.loads(path.read_text(encoding="utf-8"))
            except json.JSONDecodeError:
                pass
        time.sleep(0.1)
    raise TimeoutError("Browser fixture readiness timed out")


def sha_stream(stream):
    digest = hashlib.sha256()
    while chunk := stream.read(64 * 1024):
        digest.update(chunk)
    return digest.hexdigest()


def validate_archive(path, fixture):
    with zipfile.ZipFile(path) as archive:
        manifest = json.loads(archive.read("manifest.json"))
        assert len(manifest["novels"]) == fixture["count"]
        expected = {"manifest.json"}
        for index, novel in enumerate(manifest["novels"]):
            for file in novel["files"]:
                name = f"novels/{index}/{file['path']}"
                expected.add(name)
                assert not file["path"].startswith("raw/")
                assert archive.getinfo(name).file_size == int(file["size"])
                with archive.open(name) as stream:
                    assert sha_stream(stream) == file["sha256"], name
        assert len(archive.infolist()) == len(expected)
        assert set(archive.namelist()) == expected
        assert archive.getinfo("novels/0/client-note.bin").file_size == fixture["large_size"]
        first = next(file for file in manifest["novels"][0]["files"] if file["path"] == "client-note.bin")
        assert first["sha256"] == fixture["large_sha256"]
        assert path.stat().st_size > 10 * 1024 * 1024, "archive must exceed the gateway's single-request limit"
        return manifest


def validate_target(origin, manifest, sqlite):
    library = Path(origin["library"])
    archive_root = library / "小説データ"
    if sqlite:
        with sqlite3.connect(library / ".narou/db.sqlite") as connection:
            assert connection.execute("SELECT count(*) FROM novels").fetchone()[0] == len(manifest["novels"])
            for novel in manifest["novels"]:
                prefix = f"novels/{novel['sitename']}/{novel['ncode']}/"
                for file in novel["files"]:
                    row = connection.execute(
                        "SELECT r.blob_key,r.size,r.sha256,o.encoding,o.data FROM transfer_object_refs r "
                        "JOIN objects o ON o.object_key=r.blob_key WHERE r.object_key=?",
                        (prefix + file["path"],),
                    ).fetchone()
                    assert row is not None and row[1] == int(file["size"]) and row[2] == file["sha256"]
                    assert row[3] == "none", "published transfer stages must retain their verified raw chunk layout"
                    digest = hashlib.sha256()
                    if row[4] is not None:
                        digest.update(row[4])
                    else:
                        for chunk, in connection.execute("SELECT data FROM object_chunks WHERE object_key=? ORDER BY seq", (row[0],)):
                            digest.update(chunk)
                    assert digest.hexdigest() == file["sha256"]
        assert not any(path.name != ".objects" for path in archive_root.iterdir())
    else:
        for novel in manifest["novels"]:
            directory = archive_root / novel["sitename"] / novel["ncode"]
            for file in novel["files"]:
                path = directory / file["path"]
                assert path.stat().st_size == int(file["size"])
                with path.open("rb") as stream:
                    assert sha_stream(stream) == file["sha256"]
            assert not (directory / "raw").exists()


def main():
    ARTIFACTS.mkdir(parents=True, exist_ok=True)
    base = ROOT / "target/test-libraries"
    base.mkdir(parents=True, exist_ok=True)
    driver = None
    fixture = None
    gateway_process = None
    gateway_stats = None
    stage = "fixture startup"
    with tempfile.TemporaryDirectory(prefix="browser-controller-", dir=base) as directory:
        work = Path(directory)
        # Keep driver/cache/profile state inside the owned test tree, never in
        # the user's browser profile or a possibly read-only home cache.
        os.environ["SE_CACHE_PATH"] = str(ROOT / "target/transfer-browser-cache")
        ready = work / "fixture.json"
        env = {**os.environ, "NAROU_TRANSFER_BROWSER_READY": str(ready)}
        with (work / "fixture.log").open("wb") as fixture_log, (work / "gateway.log").open("wb") as gateway_log:
            process = subprocess.Popen(
                ["cargo", "test", "-p", "narou_rs", "--test", "web_transfer_progress", "native_transfer_browser_fixture",
                 "--", "--ignored", "--exact", "--nocapture"], cwd=ROOT, env=env, stdout=fixture_log, stderr=subprocess.STDOUT,
            )
            try:
                fixture = wait_json(ready, process)
                overall_deadline = time.monotonic() + 20 * 60 - 5
                gateway_ready, gateway_stop, gateway_stats = [work / name for name in ("gateways.json", "gateway-stop", "gateway-stats.json")]
                gateway_process = subprocess.Popen(
                    ["node", "scripts/transfer-browser-gateways.mjs", str(ready), str(gateway_ready), str(gateway_stop), str(gateway_stats)],
                    cwd=ROOT, stdout=gateway_log, stderr=subprocess.STDOUT,
                )
                bases = wait_json(gateway_ready, gateway_process)
                downloads = work / "downloads"
                downloads.mkdir()
                options = webdriver.FirefoxOptions()
                options.add_argument("-headless")
                if os.environ.get("FIREFOX_BINARY"):
                    options.binary_location = os.environ["FIREFOX_BINARY"]
                options.set_preference("browser.download.folderList", 2)
                options.set_preference("browser.download.dir", str(downloads))
                options.set_preference("browser.download.useDownloadDir", True)
                options.set_preference("browser.helperApps.neverAsk.saveToDisk", "application/zip,application/octet-stream")
                stage = "Firefox startup"
                service = webdriver.FirefoxService(
                    service_args=["--profile-root", str(work)],
                    log_output=str(work / "geckodriver.log"),
                )
                driver = webdriver.Firefox(options=options, service=service)
                driver.set_window_size(1440, 1000)
                driver.set_script_timeout(30)
                wait = WebDriverWait(driver, 240, poll_frequency=0.1)

                def click(selector):
                    wait.until(lambda browser: browser.find_element(By.CSS_SELECTOR, selector).is_displayed())
                    driver.find_element(By.CSS_SELECTOR, selector).click()

                def menu(name, action):
                    click(f'[data-dropdown="{name}"]')
                    click(f"#{action}")

                def page(index):
                    origin, base_url = fixture["origins"][index], bases[index]
                    # Authenticate the test's private browser with the same real
                    # HttpOnly cookie that the server validates. No API bypass.
                    driver.get(base_url + "/assets/js/ui/transfer.js")
                    driver.add_cookie({"name": "narou_ui_token", "value": origin["ui_token"], "path": "/", "httpOnly": True, "sameSite": "Strict"})
                    driver.get(base_url + "/")
                    wait.until(lambda browser: browser.execute_script("return document.readyState === 'complete'"))
                    # Each owned library is fresh: acknowledge the shipped
                    # first-run tour through its real UI, not by hiding it.
                    click("#feature-tour-ok")
                    wait.until(lambda browser: not browser.find_element(By.ID, "feature-tour-ok").is_displayed())
                    wait.until(lambda browser: browser.find_element(By.ID, "action-option-export").get_attribute("aria-disabled") != "true")
                    identity = driver.execute_async_script("""
                        const done = arguments[0];
                        Promise.all([fetch('/api/transfer/v2/capabilities').then(r=>r.json()),
                          fetch('/assets/transfer/manifest.json').then(r=>r.json())])
                          .then(([caps,manifest])=>done({caps,manifest}),()=>done(null));
                    """)
                    assert identity and identity["caps"]["build"]["wasm_build_id"] == identity["manifest"]["build_id"]
                    assert str(identity["caps"]["build"]["abi_version"]) == str(identity["manifest"]["abi_version"]) == "2"

                def no_transfer_error():
                    for screen in driver.find_elements(By.ID, "transfer-progress"):
                        if screen.get_attribute("data-outcome") == "error":
                            raise AssertionError(screen.text)
                    return True

                def completed():
                    no_transfer_error()
                    return driver.find_element(By.ID, "transfer-progress").get_attribute("data-outcome") == "success"

                def wait_completed():
                    last_progress = None
                    last_change = time.monotonic()

                    def progressing(browser):
                        nonlocal last_progress, last_change
                        now = time.monotonic()
                        if now - last_change >= 240:
                            raise TimeoutError(f"No byte/publication progress for 240 seconds: {last_progress}")
                        if completed():
                            return True
                        progress = browser.execute_script(r"""
                            const lines = document.querySelectorAll('#transfer-progress .transfer-progress-detail');
                            return [lines[2], lines[3]].map(line =>
                              line?.textContent.match(/\d+(?:\.\d+)?/g) || []);
                        """)
                        if progress != last_progress:
                            last_progress, last_change = progress, now
                            print(json.dumps({"stage": stage, "progress": progress}), flush=True)
                        return False

                    remaining = overall_deadline - time.monotonic()
                    if remaining <= 0:
                        raise TimeoutError("The owned browser fixture's 20-minute lifetime expired")
                    WebDriverWait(driver, remaining, poll_frequency=0.1).until(progressing)

                stage = "source list readiness"
                page(0)
                wait.until(lambda browser: f"/ {fixture['count']}" in browser.find_element(By.ID, "novel-list-info").text)
                stage = "source UI export"
                menu("select-menu", "action-select-all")
                wait.until(lambda browser: browser.find_element(By.ID, "badge-selecting").text == str(fixture["count"]))
                menu("options-menu", "action-option-export")
                wait.until(lambda browser: no_transfer_error() and browser.find_element(By.CSS_SELECTOR, ".transfer-choice-overlay .transfer-select-summary").text.startswith("168 "))
                assert driver.find_element(By.CSS_SELECTOR, '.transfer-choice-overlay input[value="runtime"]').is_selected()
                click(".transfer-choice-overlay .modal-footer .btn-primary")
                wait_completed()
                driver.save_screenshot(str(ARTIFACTS / "export.png"))

                def downloaded(_):
                    for candidate in downloads.glob("*.zip"):
                        if zipfile.is_zipfile(candidate):
                            return candidate
                    return False

                archive_path = wait.until(downloaded)
                stage = "independent ZIP reader"
                manifest = validate_archive(archive_path, fixture)
                for index, kind in [(1, "yaml"), (2, "sqlite")]:
                    stage = f"{kind} UI import"
                    page(index)
                    # WebDriver's standard file-upload command selects the
                    # actual file input; its change handler runs shipped WASM.
                    driver.find_element(By.ID, "transfer-import-file").send_keys(str(archive_path))
                    click(".transfer-choice-overlay .modal-footer .btn-primary")
                    wait.until(lambda browser: no_transfer_error() and "公開内容の確認" in browser.find_element(By.CSS_SELECTOR, ".transfer-choice-overlay").text)
                    click(".transfer-choice-overlay .modal-footer .btn-primary")
                    wait_completed()
                    assert f"新規 {fixture['count']}" in driver.find_element(By.ID, "transfer-progress-status").text
                    driver.save_screenshot(str(ARTIFACTS / f"{kind}-import.png"))
                    validate_target(fixture["origins"][index], manifest, index == 2)
                stage = "gateway bounds"
                driver.quit()
                driver = None
                gateway_stop.touch()
                assert gateway_process.wait(timeout=30) == 0
                summaries = []
                for stats in json.loads(gateway_stats.read_text(encoding="utf-8")):
                    assert stats["blocked"] == stats["omittedRecords"] == 0
                    assert stats["maxAcceptedBody"] <= 8 * 1024 * 1024
                    assert all(record["status"] != 413 and record["path"] not in ["/api/transfer/import", "/api/transfer/export"] for record in stats["records"])
                    max_response = max((record["responseBytes"] for record in stats["records"] if record["path"].startswith("/api/transfer/v2/")), default=0)
                    assert 0 < max_response <= 8 * 1024 * 1024
                    summaries.append({key: stats[key] for key in ("requests", "maxAcceptedBody", "blocked", "omittedRecords")} | {"maxTransferResponse": max_response})
                print(json.dumps({"passed": True, "browser": "Firefox", "novels": len(manifest["novels"]), "zip_bytes": archive_path.stat().st_size, "gateways": summaries}))
            except Exception as error:
                if driver:
                    driver.save_screenshot(str(ARTIFACTS / "failure.png"))
                message = str(error)
                for origin in fixture["origins"] if fixture else []:
                    message = message.replace(origin["ui_token"], "<redacted>")
                log = work / "geckodriver.log"
                if stage == "Firefox startup" and log.exists():
                    with log.open("rb") as stream:
                        stream.seek(max(0, log.stat().st_size - 16 * 1024))
                        message += "\n" + stream.read().decode("utf-8", errors="replace")
                raise RuntimeError(f"Browser transfer failed at {stage}: {message}") from None
            finally:
                if driver:
                    driver.quit()
                if gateway_process and gateway_process.poll() is None:
                    (work / "gateway-stop").touch()
                    try:
                        gateway_process.wait(timeout=15)
                    except subprocess.TimeoutExpired:
                        gateway_process.kill()
                        gateway_process.wait()
                if gateway_stats and gateway_stats.exists():
                    shutil.copyfile(gateway_stats, ARTIFACTS / "gateway-stats.json")
                if fixture:
                    Path(fixture["stop"]).touch()
                try:
                    status = process.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                else:
                    if status != 0:
                        raise RuntimeError(f"Native browser fixture exited {status}")


if __name__ == "__main__":
    main()
