const {test}=require('node:test');
const assert=require('node:assert/strict');
const fs=require('node:fs');
const vm=require('node:vm');
function harness() {
  const source=fs.readFileSync(__dirname+'/app.js','utf8'); const listeners={};
  const context=vm.createContext({listen(name,fn){listeners[name]=fn;},
    queueData:[{id:'a',status:'Pending'},{id:'b',status:'Pending'}],
    queueBody:{children:[]},currentEncodingIndex:-1,selectedRows:new Set(),refreshes:0,
    scheduleQueueRefresh(){context.refreshes++;},scheduleQueueRender(){},renderQueue(){},updateBatchButtons(){}});
  const start=source.indexOf("      listen('queue-item-updated'");
  const end=source.indexOf('// Batch started',start);
  vm.runInContext(source.slice(start,end),context);
  return {context,emit(name,payload){listeners[name]({payload});}};
}
test('probe result and status follow queue identity after reorder',()=>{
  const {context:c,emit}=harness(); c.queueData.reverse();
  emit('queue-item-probed',[0,{id:'a',videoCodec:'hevc',status:'Pending'}]);
  assert.equal(c.queueData[0].id,'b'); assert.equal(c.queueData[1].videoCodec,'hevc');
  emit('queue-item-updated',[0,'Encoding','a']);
  assert.equal(c.queueData[1].status,'Encoding'); assert.equal(c.currentEncodingIndex,1);
});
test('late events never modify a replacement at the old position',()=>{
  const {context:c,emit}=harness(); c.queueData[0]={id:'new',status:'Pending'};
  emit('queue-item-probed',[0,{id:'a',videoCodec:'hevc'}]);
  emit('queue-item-updated',[0,'Failed','a']);
  assert.equal(c.queueData[0].id,'new'); assert.equal(c.queueData[0].status,'Pending');
  assert.equal(c.refreshes,2);
});
