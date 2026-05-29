export function handle(_req,_r,_g){
  return { status: 429, headers: [["content-type","application/json"]],
    body: new TextEncoder().encode(JSON.stringify({error:{message:"Rate limit reached",type:"rate_limit_error",code:"rate_limit_exceeded",param:null}})) };
}