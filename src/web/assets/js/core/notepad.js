import { fetchJson, postJson, assertApiSuccess } from './http.js';

function readSnapshot(data) {
  return {
    text: typeof data === 'string' ? data : String(data?.text ?? data?.content ?? ''),
    objectId: typeof data?.object_id === 'string' ? data.object_id : null,
  };
}

export function createNotepadEditor(textarea) {
  let baseline = '';
  let objectId = null;
  let initialized = false;
  let revision = 0;
  let readGeneration = 0;
  let loading = null;
  let saveTail = Promise.resolve();
  let pendingSaves = 0;
  let conflict = null;

  textarea.addEventListener('input', () => { revision++; });

  const isDirty = () => initialized ? textarea.value !== baseline : revision !== 0;
  const acknowledge = (text, id) => {
    baseline = text;
    objectId = id;
    initialized = true;
  };

  function apply(data, editedSinceRead = false) {
    const snapshot = readSnapshot(data);
    const changed = textarea.value !== snapshot.text;
    const keepDraft = editedSinceRead || isDirty() || pendingSaves > 0;
    if (!initialized) {
      acknowledge(snapshot.text, snapshot.objectId);
    } else if (keepDraft) {
      return { ...snapshot, keptLocalEdits: changed, applied: false };
    } else {
      acknowledge(snapshot.text, snapshot.objectId);
    }
    if (!keepDraft) textarea.value = snapshot.text;
    return { ...snapshot, keptLocalEdits: keepDraft && changed, applied: !keepDraft && changed };
  }

  function applySnapshot(data) {
    ++readGeneration;
    return apply(data);
  }

  function load() {
    const generation = ++readGeneration;
    const capturedRevision = revision;
    const capturedText = textarea.value;
    const promise = fetchJson('/api/notepad/read').then(data => {
      if (generation !== readGeneration) return null;
      return apply(data, capturedRevision !== revision || capturedText !== textarea.value);
    });
    loading = promise;
    promise.finally(() => { if (loading === promise) loading = null; }).catch(() => {});
    return promise;
  }

  function save() {
    const submitted = textarea.value;
    pendingSaves++;
    const work = saveTail.then(async () => {
      if (conflict) throw conflict;
      if (!initialized) {
        await (loading || load());
        if (!initialized) throw new Error('メモ帳の読み込みが完了していません');
      }
      if (submitted === baseline) return { saved: false, dirty: isDirty(), data: null };
      let data;
      try {
        data = await postJson('/api/notepad/save', {
          text: submitted,
          object_id: objectId,
        });
        if (data?.conflict) {
          const error = new Error(data.message || '他の画面でメモ帳が変更されています');
          error.status = 409;
          error.data = data;
          error.code = data.error?.code || data.code || '';
          throw error;
        }
        assertApiSuccess(data, 'メモ帳の保存に失敗しました');
      } catch (error) {
        if (error.status === 409 || error.data?.conflict) {
          error.message = '他の画面でメモ帳が変更されています。入力内容は保持しました。内容を控えてから再読み込みしてください';
          conflict = error;
        }
        throw error;
      }
      ++readGeneration;
      acknowledge(submitted, readSnapshot(data).objectId);
      return { saved: true, dirty: isDirty(), data };
    }).finally(() => { pendingSaves--; });
    saveTail = work.catch(() => {});
    return work;
  }

  return { applySnapshot, load, save, isDirty, invalidateReads: () => { ++readGeneration; } };
}
