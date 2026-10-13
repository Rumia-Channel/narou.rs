import { State } from './state.js';
import { fetchJson, postJson, assertApiSuccess } from './http.js';


let operationTail = Promise.resolve();
let sortTouched = false;
let columns = null;
let columnsLoad = null;

function enqueue(action) {
  const work = operationTail.then(action);
  operationTail = work.catch(() => {});
  return work;
}

function captureSort() {
  return { key: State.sortCol, dir: State.sortAsc ? 'asc' : 'desc' };
}

async function acknowledgeSort(sort) {
  if (!columns) {
    columnsLoad ||= fetchJson('/api/sort_state').then(data => {
      restoreSortState(data);
      if (!columns) throw new Error('並び順の定義を取得できませんでした');
    }).finally(() => { columnsLoad = null; });
    await columnsLoad;
  }
  const column = columns.indexOf(sort.key);
  if (column < 0) throw new Error('並び順が不正です');
  const result = await postJson('/api/sort_state', { column, dir: sort.dir });
  assertApiSuccess(result, '並び順の保存に失敗しました');
}

export function persistSortState() {
  sortTouched = true;
  const sort = captureSort();
  return enqueue(() => acknowledgeSort(sort));
}

export function postSortedJson(url, body) {
  sortTouched = true;
  const sort = captureSort();
  const timestamp = Date.now();
  return enqueue(async () => {
    await acknowledgeSort(sort);
    return postJson(url, { ...body, timestamp });
  });
}

export function restoreSortState(data) {
  if (Array.isArray(data?.columns)) columns = data.columns;
  if (sortTouched || !data || !columns) return;
  const column = columns[Number(data.column)];
  if (!column) return;
  State.sortCol = column;
  State.sortAsc = data.dir === 'asc';
}
