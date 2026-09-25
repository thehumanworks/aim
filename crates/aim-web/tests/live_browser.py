"""Live headless OpenRouter browser smoke. Usage: python live_browser.py PORT TOKEN_FILE WORKSPACE."""

import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

port, token_file, workspace = sys.argv[1:]
token = Path(token_file).read_text().strip()
marker = "W24_ALIVE_6B8F"
prompt = f"Reply with exactly {marker} and nothing else."
script = r'''(() => {
  sessionStorage.setItem('aim-daemon-token', TOKEN);
  const workspace = WORKSPACE;
  const prompt = PROMPT;
  const marker = MARKER;
  let stage = 0, sent = 0, first = 0;
  const put = (label, value) => {
    const input = document.querySelector(label);
    if (!input) return false;
    input.value = value;
    input.dispatchEvent(new Event('input', { bubbles: true }));
    return true;
  };
  const submit = (selector) => {
    const form = document.querySelector(selector);
    if (!form) return false;
    form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
    return true;
  };
  const finish = () => {
    const result = document.createElement('div');
    result.id = 'w24-result';
    result.textContent = `result=streamed-answer first_delta_ms=${first} final_ms=${Date.now()-sent} provider=openrouter`;
    document.body.append(result);
    window.__w24done = true;
  };
  setInterval(() => {
    try {
      if (stage === 0 && document.querySelector('.status')?.textContent?.includes('Connected')) {
        if (put('input[aria-label="Workspace"]', workspace) && put('input[aria-label="Provider"]', 'openrouter') && submit('form.create')) stage = 1;
      } else if (stage === 1 && document.querySelector('textarea[aria-label="Message"]') && document.querySelector('.main strong')?.textContent !== 'No session') {
        if (put('textarea[aria-label="Message"]', prompt) && submit('form.composer')) { stage = 2; sent = Date.now(); }
      } else if (stage === 2) {
        const entries = Array.from(document.querySelectorAll('.transcript .entry.assistant'));
        const streaming = entries[entries.length - 1]?.textContent || '';
        if (streaming && !first) first = Date.now() - sent;
        if (entries.slice(0, -1).some(e => e.textContent.includes(marker)) && !streaming) finish();
      }
    } catch (error) {
      const result = document.createElement('div');
      result.id = 'w24-result';
      result.textContent = `browser-script-error=${error.name}`;
      document.body.append(result);
      window.__w24done = true;
    }
  }, 100);
})();'''.replace('TOKEN',json.dumps(token)).replace('WORKSPACE',json.dumps(workspace)).replace('PROMPT',json.dumps(prompt)).replace('MARKER',json.dumps(marker))
with tempfile.NamedTemporaryFile(mode='w', prefix='w24-browser-', suffix='.js', delete=False) as script_file:
    os.chmod(script_file.name, 0o600)
    script_file.write(script)
    script_path = script_file.name
try:
    cmd = ['mise','exec','--','lightpanda','fetch',f'http://127.0.0.1:{port}/','--inject-script-file',script_path,'--wait-script','window.__w24done === true','--wait-ms','120000','--terminate-ms','130000','--dump-selector','#w24-result','--dump','markdown']
    completed = subprocess.run(cmd, capture_output=True, text=True, timeout=140)
    output = (completed.stdout + completed.stderr).replace(token, '[REDACTED]')
    match = re.search(r'result=streamed\\?-answer first\\?_delta\\?_ms=(\d+) final\\?_ms=(\d+) provider=openrouter', output)
    if completed.returncode == 0 and match and int(match.group(1)) > 0:
        print(f'first_delta_ms={match.group(1)} final_ms={match.group(2)} provider=openrouter')
    else:
        print(f'headless_browser_failed exit={completed.returncode}')
        sys.exit(1)
finally:
    os.unlink(script_path)
