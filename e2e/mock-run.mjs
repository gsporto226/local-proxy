import { startMockUpstream } from "./mock-upstream.ts";
const m = await startMockUpstream();
console.log("MOCK_PORT=" + m.port);
setInterval(()=>{}, 1000);
