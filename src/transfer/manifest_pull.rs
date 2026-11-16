//! Durable JSON emission from independently stored UTF-8 metadata fragments.
//! A request identifies one <=32 KiB raw fragment, never a serialized prefix.
use super::{Result, TransferError};

#[derive(Debug,Clone,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
pub struct NovelJsonLayout { pub novel_type:u8, pub optional:u16, pub tags:u32, pub members:Option<u32> }
impl NovelJsonLayout {
    pub fn fields(&self)->Result<u32>{10u32.checked_add(self.tags).and_then(|count|count.checked_add(self.members.unwrap_or(0))).ok_or_else(invalid)}
    pub fn present(&self,field:u32)->bool{if field<10{self.optional&(1<<field)!=0}else{self.fields().is_ok_and(|count|field<count)}}
}
#[derive(Debug,Clone,Copy,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
pub struct NovelJsonRequest {pub field:u32,pub part:u32}
pub struct NovelJsonText<'a> {pub field:u32,pub part:u32,pub text:&'a str,pub last:bool}
pub struct NovelJsonOutput {pub bytes:Vec<u8>,pub done:bool,pub request:Option<NovelJsonRequest>}

#[derive(Debug,Clone,Copy,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
enum Phase {Start,Scalar,Open,Text,Close,Tags,Tag,TagsEnd,Number,Members,Member,MembersEnd,End,Done}
#[derive(Debug,Clone,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NovelJsonPullCursor {version:u8,phase:Phase,field:u32,item:u32,part:u32,byte:usize,token:usize,escape:usize,section:u8}
impl Default for NovelJsonPullCursor {fn default()->Self{Self{version:1,phase:Phase::Start,field:0,item:0,part:0,byte:0,token:0,escape:0,section:0}}}
fn invalid()->TransferError{TransferError::Protocol("invalid fragmented novel metadata cursor".into())}
fn prefix(field:u32)->&'static [u8]{match field{0=>b"\"toc_url\":",1=>b",\"title\":",2=>b",\"author\":",3=>b",\"sitename\":",4=>b",\"domain\":",5=>b",\"attribute_tag\":",6=>b",\"ncode\":",7=>b",\"subtitle\":",8=>b",\"general_lastup\":",9=>b",\"last_update\":",_=>b""}}
fn token(out:&mut Vec<u8>,limit:usize,cursor:&mut NovelJsonPullCursor,bytes:&[u8])->Result<bool>{
    if cursor.token>bytes.len(){return Err(invalid());}let count=(limit-out.len()).min(bytes.len()-cursor.token);out.extend_from_slice(&bytes[cursor.token..cursor.token+count]);cursor.token+=count;
    if cursor.token==bytes.len(){cursor.token=0;Ok(true)}else{Ok(false)}
}

/// Emit at most64KiB. Supply the requested fragment on the next call; the
/// cursor retains token, escape and input byte offsets across any boundary.
pub fn novel_json_pull(layout:&NovelJsonLayout,cursor:&mut NovelJsonPullCursor,input:Option<NovelJsonText<'_>>,limit:usize)->Result<NovelJsonOutput>{
    if limit==0||limit>super::protocol::IO_CHUNK_BYTES||cursor.version!=1||layout.optional&0xf!=0xf||layout.optional&!0x3ff!=0||cursor.section>2{return Err(invalid());}
    layout.fields()?;
    if input.as_ref().is_some_and(|input|input.text.len()>super::protocol::TEXT_FRAGMENT_MAX_BYTES||(!input.last&&input.text.is_empty())){return Err(invalid());}
    let mut bytes=Vec::with_capacity(limit);let mut request=None;
    while bytes.len()<limit&&cursor.phase!=Phase::Done {
        match cursor.phase{
            Phase::Start=>if token(&mut bytes,limit,cursor,b"{")?{cursor.phase=Phase::Scalar;},
            Phase::Scalar=>{
                while cursor.field<10&&!layout.present(cursor.field){cursor.field+=1;}
                if cursor.field==10{cursor.phase=Phase::Tags;continue;}
                if cursor.field>10{return Err(invalid());}
                if token(&mut bytes,limit,cursor,prefix(cursor.field))?{cursor.section=0;cursor.phase=Phase::Open;}
            },
            Phase::Open=>if token(&mut bytes,limit,cursor,b"\"")?{cursor.phase=Phase::Text;},
            Phase::Text=>{
                let field=match cursor.section{0=>cursor.field,1=>10u32.checked_add(cursor.item).ok_or_else(invalid)?,2=>10u32.checked_add(layout.tags).and_then(|count|count.checked_add(cursor.item)).ok_or_else(invalid)?,_=>return Err(invalid())};
                let wanted=NovelJsonRequest{field,part:cursor.part};
                let Some(input)=input.as_ref().filter(|input|input.field==field&&input.part==cursor.part)else{request=Some(wanted);break};
                if cursor.byte>input.text.len(){return Err(invalid());}
                if cursor.byte==input.text.len(){
                    if cursor.escape!=0{return Err(invalid());}
                    cursor.byte=0;if input.last{cursor.part=0;cursor.phase=Phase::Close;}else{cursor.part=cursor.part.checked_add(1).ok_or_else(invalid)?;request=Some(NovelJsonRequest{field,part:cursor.part});break;}continue;
                }
                let byte=input.text.as_bytes()[cursor.byte];let mut encoded=[0;6];
                if let Some(escape)=super::manifest_write::json_escape(byte,&mut encoded){
                    if cursor.escape>=escape.len(){return Err(invalid());}let count=(limit-bytes.len()).min(escape.len()-cursor.escape);bytes.extend_from_slice(&escape[cursor.escape..cursor.escape+count]);cursor.escape+=count;
                    if cursor.escape==escape.len(){cursor.escape=0;cursor.byte+=1;}
                }else{if cursor.escape!=0{return Err(invalid());}let start=cursor.byte;let remaining=(limit-bytes.len()).min(input.text.len()-start);let mut count=0;while count<remaining&&super::manifest_write::json_escape(input.text.as_bytes()[start+count],&mut encoded).is_none(){count+=1;}
                    bytes.extend_from_slice(&input.text.as_bytes()[start..start+count]);cursor.byte+=count;
                }
            },
            Phase::Close=>if token(&mut bytes,limit,cursor,b"\"")?{match cursor.section{0=>{cursor.field+=1;cursor.phase=Phase::Scalar;},1=>{cursor.item+=1;cursor.phase=Phase::Tag;},2=>{cursor.item+=1;cursor.phase=Phase::Member;},_=>return Err(invalid())}},
            Phase::Tags=>if token(&mut bytes,limit,cursor,b",\"tags\":[")?{cursor.item=0;cursor.phase=Phase::Tag;},
            Phase::Tag=>{if cursor.item>=layout.tags{if cursor.item!=layout.tags{return Err(invalid());}cursor.phase=Phase::TagsEnd;continue;}
                if cursor.item==0||token(&mut bytes,limit,cursor,b",")?{cursor.section=1;cursor.phase=Phase::Open;}},
            Phase::TagsEnd=>if token(&mut bytes,limit,cursor,b"]")?{cursor.phase=Phase::Number;},
            Phase::Number=>{let mut digits=[0u8;20];let number=super::manifest_write::decimal(layout.novel_type as u64,&mut digits);let mut value=[0u8;22];let name=b",\"novel_type\":";value[..name.len()].copy_from_slice(name);value[name.len()..name.len()+number.len()].copy_from_slice(number);if token(&mut bytes,limit,cursor,&value[..name.len()+number.len()])?{cursor.phase=if layout.members.is_some(){Phase::Members}else{Phase::End};}},
            Phase::Members=>if token(&mut bytes,limit,cursor,b",\"virtual_members\":[")?{cursor.item=0;cursor.phase=Phase::Member;},
            Phase::Member=>{let count=layout.members.ok_or_else(invalid)?;if cursor.item>=count{if cursor.item!=count{return Err(invalid());}cursor.phase=Phase::MembersEnd;continue;}
                if cursor.item==0||token(&mut bytes,limit,cursor,b",")?{cursor.section=2;cursor.phase=Phase::Open;}},
            Phase::MembersEnd=>if token(&mut bytes,limit,cursor,b"]")?{cursor.phase=Phase::End;},
            Phase::End=>if token(&mut bytes,limit,cursor,b"}")?{cursor.phase=Phase::Done;},
            Phase::Done=>{},
        }
    }
    Ok(NovelJsonOutput{bytes,done:cursor.phase==Phase::Done,request})
}

#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn fragmented_metadata_roundtrips_at_every_token_and_escape_boundary(){
        let layout=NovelJsonLayout{novel_type:2,optional:0x3ff,tags:2,members:Some(1)};
        let fields=["https://example.test/日本語","\0\u{1}日本語\\\"","author","site","domain","attribute","ncode","story","date","updated","tag1","tag2","member"];
        for limit in 1..=96{
            let mut cursor=NovelJsonPullCursor::default();let mut all=Vec::new();let mut requested=None;let mut done=false;
            while !done{
                let input=requested.map(|request:NovelJsonRequest|NovelJsonText{field:request.field,part:request.part,text:fields[request.field as usize],last:true});
                let next=novel_json_pull(&layout,&mut cursor,input,limit).unwrap();assert!(next.bytes.len()<=limit);all.extend(next.bytes);done=next.done;if next.request.is_some(){requested=next.request;}
                cursor=serde_json::from_slice(&serde_json::to_vec(&cursor).unwrap()).unwrap();
            }
            let value:serde_json::Value=serde_json::from_slice(&all).unwrap();assert_eq!(value["title"],fields[1]);assert_eq!(value["subtitle"],"story");assert_eq!(value["tags"],serde_json::json!(["tag1","tag2"]));assert_eq!(value["virtual_members"],serde_json::json!(["member"]));
        }
    }
    #[test]
    fn large_escaped_field_requests_each_raw_fragment_once_without_prefix_replay(){
        let layout=NovelJsonLayout{novel_type:1,optional:15,tags:0,members:None};let part="\0".repeat(super::super::protocol::TEXT_FRAGMENT_MAX_BYTES);let mut cursor=NovelJsonPullCursor::default();let mut requested=None;let mut requested_parts=Vec::new();let mut output_bytes=0u64;
        loop{
            let input=requested.map(|request:NovelJsonRequest|NovelJsonText{field:request.field,part:request.part,text:if request.field==1{&part}else{""},last:request.field!=1||request.part==191});
            let result=novel_json_pull(&layout,&mut cursor,input,65536).unwrap();output_bytes+=result.bytes.len() as u64;
            if let Some(request)=result.request{if request.field==1{requested_parts.push(request.part);}requested=Some(request);}
            if result.done{break;}
        }
        assert_eq!(requested_parts,(0..192).collect::<Vec<_>>());assert!(output_bytes>36*1024*1024);assert!(serde_json::to_vec(&cursor).unwrap().len()<256);
    }
}
