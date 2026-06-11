//! Per-instance, per-session deterministic noise injected into fingerprintable
//! surfaces (the Brave "farbling" model).
//!
//! Each identity carries its own seed, so the three heads do not correlate. The
//! goal is to stop a tracker building a stable cross-site identity of the active
//! head. This is randomize-*our-own-surface*, never impersonation of another
//! browser or device (see the threat model's non-goals).
//!
//! The perturbation is deterministic given `(seed, channel, index)` and bounded
//! to ±1 per byte, so output still renders correctly. The actual JS-side shims
//! (canvas, audio, WebGL, font metrics) are emitted by [`FarblingProvider::js_prologue`]
//! and injected via the `JsEngine` seam; the real shim bodies land at M6.

/// A fingerprintable surface that farbling perturbs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// `canvas.toDataURL` / `getImageData`.
    Canvas,
    /// `AudioContext` sample data.
    Audio,
    /// WebGL `readPixels` / parameters.
    WebglReadPixels,
    /// Font metrics (`measureText`, bounding boxes).
    FontMetrics,
}

impl Channel {
    /// A stable per-channel tag mixed into the noise function.
    fn tag(self) -> u64 {
        match self {
            Channel::Canvas => 0x01,
            Channel::Audio => 0x02,
            Channel::WebglReadPixels => 0x03,
            Channel::FontMetrics => 0x04,
        }
    }
}

/// Supplies per-head fingerprint noise and the JS prologue that installs the
/// browser-side shims. One implementation per head (distinct seeds).
pub trait FarblingProvider: Send {
    /// The head's farbling seed.
    fn seed(&self) -> u64;

    /// Deterministically perturb one byte of a fingerprintable read. Bounded to
    /// ±1 so the surface still renders/sounds correct.
    fn perturb(&self, channel: Channel, index: u64, value: u8) -> u8;

    /// The JavaScript prologue installing the fingerprint shims for this head.
    /// Injected into each realm before page scripts run.
    fn js_prologue(&self) -> String;
}

/// Deterministic, seeded farbling using a SplitMix64 mixer.
#[derive(Clone, Copy, Debug)]
pub struct SeededFarbling {
    seed: u64,
}

impl SeededFarbling {
    /// Create a provider for a head's seed.
    pub fn new(seed: u64) -> Self {
        Self { seed }
    }
}

impl FarblingProvider for SeededFarbling {
    fn seed(&self) -> u64 {
        self.seed
    }

    fn perturb(&self, channel: Channel, index: u64, value: u8) -> u8 {
        let mixed = splitmix64(
            self.seed
                ^ channel.tag().wrapping_mul(0x9E37_79B9_7F4A_7C15)
                ^ index.wrapping_mul(0xD1B5_4A32_D192_ED03),
        );
        // Map to a delta in {-1, 0, +1}: mostly perturb, occasionally leave be.
        let delta: i8 = match mixed % 3 {
            0 => -1,
            1 => 0,
            _ => 1,
        };
        value.saturating_add_signed(delta)
    }

    fn js_prologue(&self) -> String {
        // The M6 shims: canvas 2D, audio, WebGL, and font metrics, all driven
        // by a per-head PRNG seeded from this head's seed. Deterministic per
        // (head, inputs); uncorrelated across heads. See FARBLING_SHIMS.
        let hi = (self.seed >> 32) as u32;
        let lo = self.seed as u32;
        format!("(function(){{var __FARBLE_HI={hi},__FARBLE_LO={lo};\n{FARBLING_SHIMS}\n}})();\n")
    }
}

/// The JS fingerprint shims (M6). Installed into every realm before the DOM
/// model and page scripts. Design notes:
///
/// - **canvas 2D**: draw calls append to an op log; `getImageData`/`toDataURL`
///   readbacks are synthesized from a PRNG keyed by (head seed, op log, dims) —
///   stable for one head, divergent across heads, so a canvas hash cannot
///   correlate identities. `toDataURL` emits a *real* PNG (stored-block
///   deflate + CRC/Adler in JS) so format sniffers pass.
/// - **WebGL**: `VENDOR`/`RENDERER` answer a uniform "Cerberus" for every
///   head (uniformity beats inventing fake GPUs); `readPixels` — the actual
///   entropy surface — returns per-head seeded noise.
/// - **audio**: analyser/offline-render readbacks return near-silence with
///   per-head noise in the low bits, deterministic per head.
/// - **font metrics**: `measureText` widths get a bounded (≤2%) per-(head,
///   font, text) jitter; metric probing of installed fonts is useless anyway
///   since Cerberus only ships bundled fonts.
const FARBLING_SHIMS: &str = r##"
  function __fnv(s){var h=2166136261>>>0;for(var i=0;i<s.length;i++){h=Math.imul(h^s.charCodeAt(i),16777619)>>>0;}return h>>>0;}
  function __rng(ch,key){
    var s=(__FARBLE_LO ^ Math.imul(ch,0x9E3779B9) ^ __fnv(key||""))>>>0;
    var t=(__FARBLE_HI ^ Math.imul(ch,0x85EBCA6B))>>>0;
    return function(){
      s=(s+0x9E3779B9)>>>0; var z=(s^t)>>>0;
      z^=z>>>16; z=Math.imul(z,0x85EBCA6B)>>>0;
      z^=z>>>13; z=Math.imul(z,0xC2B2AE35)>>>0;
      z^=z>>>16; return z>>>0;
    };
  }

  // ---- PNG writer (stored-block zlib; real, decodable output) ----
  var __CRC_T=(function(){var t=[];for(var n=0;n<256;n++){var c=n;for(var k=0;k<8;k++)c=(c&1)?((0xEDB88320^(c>>>1))>>>0):(c>>>1);t[n]=c>>>0;}return t;})();
  function __crc32(b,s,e){var c=0xFFFFFFFF;for(var i=s;i<e;i++)c=(__CRC_T[(c^b[i])&255]^(c>>>8))>>>0;return (c^0xFFFFFFFF)>>>0;}
  function __png(w,h,rgba){
    var stride=w*4+1, raw=new Uint8Array(stride*h);
    for(var y=0;y<h;y++){raw[y*stride]=0;raw.set(rgba.subarray(y*w*4,(y+1)*w*4),y*stride+1);}
    var nb=Math.max(1,Math.ceil(raw.length/65535));
    var z=new Uint8Array(2+raw.length+5*nb+4), zi=0;
    z[zi++]=0x78;z[zi++]=0x01;
    var off=0;
    for(var b=0;b<nb;b++){
      var len=Math.min(65535,raw.length-off), last=(b===nb-1)?1:0;
      z[zi++]=last;z[zi++]=len&255;z[zi++]=(len>>>8)&255;z[zi++]=(~len)&255;z[zi++]=((~len)>>>8)&255;
      z.set(raw.subarray(off,off+len),zi);zi+=len;off+=len;
    }
    var a=1,bb=0;for(var i=0;i<raw.length;i++){a=(a+raw[i])%65521;bb=(bb+a)%65521;}
    var ad=(((bb<<16)>>>0)|a)>>>0;
    z[zi++]=(ad>>>24)&255;z[zi++]=(ad>>>16)&255;z[zi++]=(ad>>>8)&255;z[zi++]=ad&255;
    function be32(n){return [(n>>>24)&255,(n>>>16)&255,(n>>>8)&255,n&255];}
    function chunk(type,data){
      var out=[].concat(be32(data.length),[type.charCodeAt(0),type.charCodeAt(1),type.charCodeAt(2),type.charCodeAt(3)]);
      for(var i=0;i<data.length;i++)out.push(data[i]);
      var buf=new Uint8Array(out.slice(4));
      out=out.concat(be32(__crc32(buf,0,buf.length)));
      return out;
    }
    var ihdr=[].concat(be32(w),be32(h),[8,6,0,0,0]);
    var bytes=[137,80,78,71,13,10,26,10]
      .concat(chunk("IHDR",ihdr))
      .concat(chunk("IDAT",Array.prototype.slice.call(z.subarray(0,zi))))
      .concat(chunk("IEND",[]));
    return bytes;
  }
  var __B64="ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  function __b64(bytes){
    var out="";
    for(var i=0;i<bytes.length;i+=3){
      var a=bytes[i],b=bytes[i+1],c=bytes[i+2];
      out+=__B64[a>>>2]+__B64[((a&3)<<4)|((b||0)>>>4)];
      out+=(i+1<bytes.length)?__B64[((b&15)<<2)|((c||0)>>>6)]:"=";
      out+=(i+2<bytes.length)?__B64[c&63]:"=";
    }
    return out;
  }

  // ---- font metrics ----
  function __measure(t,font){
    var px=parseFloat(font)||10;
    var base=0;for(var i=0;i<t.length;i++){base+=(t.charCodeAt(i)===32)?0.33:0.6;}
    base*=px;
    var r=__rng(4,String(font)+"|"+t);
    var width=base+((r()%1000)/1000)*0.02*base;
    return {width:width,
      actualBoundingBoxLeft:0,actualBoundingBoxRight:width,
      actualBoundingBoxAscent:px*0.8,actualBoundingBoxDescent:px*0.2,
      fontBoundingBoxAscent:px*0.8,fontBoundingBoxDescent:px*0.25};
  }

  // ---- canvas ----
  function __dims(el){
    var w=parseInt(el.width,10)||(el.getAttribute&&parseInt(el.getAttribute("width"),10))||300;
    var h=parseInt(el.height,10)||(el.getAttribute&&parseInt(el.getAttribute("height"),10))||150;
    return [Math.max(1,Math.min(4096,w)),Math.max(1,Math.min(4096,h))];
  }
  function __noiseRGBA(r,n){
    var d=new Uint8Array(n*4);
    for(var i=0;i<d.length;i+=4){var v=r();d[i]=v&255;d[i+1]=(v>>>8)&255;d[i+2]=(v>>>16)&255;d[i+3]=255;}
    return d;
  }
  function __dataURL(el){
    var wh=__dims(el),w=wh[0],h=wh[1];
    var area=w*h,scale=area>65536?Math.sqrt(65536/area):1;
    var ew=Math.max(1,Math.floor(w*scale)),eh=Math.max(1,Math.floor(h*scale));
    var r=__rng(1,(el.__cerbOps||"")+"|"+w+"x"+h);
    return "data:image/png;base64,"+__b64(__png(ew,eh,__noiseRGBA(r,ew*eh)));
  }
  function __make2D(el){
    if(el.__cerb2d)return el.__cerb2d;
    function log(s){el.__cerbOps=(el.__cerbOps||"")+s;}
    function logger(tag){return function(){log(tag+Array.prototype.join.call(arguments,","));};}
    var ctx={canvas:el,fillStyle:"#000",strokeStyle:"#000",lineWidth:1,
      font:"10px sans-serif",textBaseline:"alphabetic",textAlign:"start",
      globalAlpha:1,globalCompositeOperation:"source-over",
      fillRect:function(){log("fR"+Array.prototype.join.call(arguments,",")+this.fillStyle);},
      strokeRect:logger("sR"),clearRect:logger("cR"),
      fillText:function(t,x,y){log("fT"+t+","+x+","+y+","+this.font+","+this.fillStyle);},
      strokeText:logger("sT"),
      beginPath:logger("bP"),closePath:logger("cP"),
      moveTo:logger("m"),lineTo:logger("l"),arc:logger("a"),arcTo:logger("at"),
      ellipse:logger("e"),bezierCurveTo:logger("bz"),quadraticCurveTo:logger("qd"),
      rect:logger("r"),fill:function(){log("F"+this.fillStyle);},
      stroke:function(){log("S"+this.strokeStyle);},
      save:function(){},restore:function(){},clip:function(){},
      rotate:logger("ro"),translate:logger("tr"),scale:logger("sc"),
      transform:logger("tf"),setTransform:function(){},resetTransform:function(){},
      drawImage:logger("dI"),putImageData:logger("pID"),
      setLineDash:function(){},getLineDash:function(){return [];},
      createLinearGradient:function(){return {addColorStop:function(){}};},
      createRadialGradient:function(){return {addColorStop:function(){}};},
      createPattern:function(){return null;},
      isPointInPath:function(){return false;},
      createImageData:function(w,h){w=Math.max(1,w|0);h=Math.max(1,h|0);
        return {width:w,height:h,data:new Uint8ClampedArray(w*h*4)};},
      getImageData:function(x,y,w,h){
        w=Math.max(1,Math.min(4096,w|0));h=Math.max(1,Math.min(4096,h|0));
        var r=__rng(1,(el.__cerbOps||"")+"|gID"+x+","+y+","+w+"x"+h);
        return {width:w,height:h,data:new Uint8ClampedArray(__noiseRGBA(r,w*h))};
      },
      measureText:function(t){return __measure(String(t),this.font);}
    };
    el.__cerb2d=ctx;return ctx;
  }
  function __makeWebGL(el){
    if(el.__cerbGL)return el.__cerbGL;
    var wh=__dims(el);
    var gl={canvas:el,drawingBufferWidth:wh[0],drawingBufferHeight:wh[1],
      getParameter:function(p){
        switch(p|0){
          case 0x1F00: return "Cerberus";
          case 0x1F01: return "Cerberus Software Renderer";
          case 0x1F02: return "WebGL 1.0 (Cerberus)";
          case 0x8B8C: return "WebGL GLSL ES 1.0 (Cerberus)";
          case 0x9245: return "Cerberus";
          case 0x9246: return "Cerberus Software Renderer";
          case 0x0D33: case 0x851C: case 0x8869: case 0x8DFB: return 4096;
          default: return 0;
        }
      },
      getSupportedExtensions:function(){return ["OES_texture_float","OES_element_index_uint"];},
      getExtension:function(name){
        if(name==="WEBGL_debug_renderer_info")return {UNMASKED_VENDOR_WEBGL:0x9245,UNMASKED_RENDERER_WEBGL:0x9246};
        return null;
      },
      readPixels:function(x,y,w,h,fmt,type,out){
        if(out&&out.length){var r=__rng(3,"rp|"+x+","+y+","+w+"x"+h);
          for(var i=0;i<out.length;i++)out[i]=r()&255;}
      },
      getContextAttributes:function(){return {alpha:true,antialias:true,depth:true};},
      getShaderPrecisionFormat:function(){return {rangeMin:127,rangeMax:127,precision:23};},
      createShader:function(){return {};},createProgram:function(){return {};},
      shaderSource:function(){},compileShader:function(){},attachShader:function(){},
      linkProgram:function(){},useProgram:function(){},deleteShader:function(){},
      getShaderParameter:function(){return true;},getProgramParameter:function(){return true;},
      createBuffer:function(){return {};},bindBuffer:function(){},bufferData:function(){},
      enableVertexAttribArray:function(){},vertexAttribPointer:function(){},
      drawArrays:function(){},drawElements:function(){},clear:function(){},
      clearColor:function(){},viewport:function(){},enable:function(){},disable:function(){},
      getError:function(){return 0;},finish:function(){},flush:function(){}
    };
    el.__cerbGL=gl;return gl;
  }
  function __attachCanvas(el){
    if(el.__cerbCanvas)return el;
    el.__cerbCanvas=true;
    if(el.width===undefined)el.width=300;
    if(el.height===undefined)el.height=150;
    el.getContext=function(kind){
      kind=String(kind||"2d").toLowerCase();
      if(kind.indexOf("webgl")===0||kind==="experimental-webgl")return __makeWebGL(el);
      if(kind==="2d")return __make2D(el);
      return null;
    };
    el.toDataURL=function(){return __dataURL(el);};
    el.toBlob=function(cb){var u=__dataURL(el);
      if(typeof cb==="function")cb({size:u.length,type:"image/png"});};
    return el;
  }

  // ---- audio ----
  function __makeAnalyser(){
    return {fftSize:2048,frequencyBinCount:1024,smoothingTimeConstant:0.8,
      minDecibels:-100,maxDecibels:-30,
      connect:function(n){return n;},disconnect:function(){},
      getFloatFrequencyData:function(a){var r=__rng(2,"ff"+a.length);
        for(var i=0;i<a.length;i++)a[i]=-100+((r()%2000)/1000);},
      getByteFrequencyData:function(a){var r=__rng(2,"bf"+a.length);
        for(var i=0;i<a.length;i++)a[i]=r()&3;},
      getFloatTimeDomainData:function(a){var r=__rng(2,"ft"+a.length);
        for(var i=0;i<a.length;i++)a[i]=(((r()%2000)/1000)-1)*0.001;},
      getByteTimeDomainData:function(a){var r=__rng(2,"bt"+a.length);
        for(var i=0;i<a.length;i++)a[i]=128+(r()%3)-1;}
    };
  }
  function __makeAudioBuffer(ch,len,rate){
    ch=Math.max(1,ch|0);len=Math.max(1,len|0);rate=rate||44100;
    return {numberOfChannels:ch,length:len,sampleRate:rate,duration:len/rate,
      getChannelData:function(c){var r=__rng(2,"chan"+c+"|"+len);
        var a=new Float32Array(len);
        for(var i=0;i<len;i++)a[i]=(((r()%2000)/1000)-1)*1e-4;
        return a;}
    };
  }
  function __AudioCtx(){
    this.destination={connect:function(n){return n;},disconnect:function(){},maxChannelCount:2};
    this.sampleRate=44100;this.state="running";this.currentTime=0;
  }
  __AudioCtx.prototype.createAnalyser=function(){return __makeAnalyser();};
  __AudioCtx.prototype.createOscillator=function(){
    return {type:"sine",frequency:{value:440},detune:{value:0},
      connect:function(n){return n;},disconnect:function(){},start:function(){},stop:function(){},onended:null};
  };
  __AudioCtx.prototype.createDynamicsCompressor=function(){
    return {threshold:{value:-24},knee:{value:30},ratio:{value:12},
      attack:{value:0.003},release:{value:0.25},reduction:0,
      connect:function(n){return n;},disconnect:function(){}};
  };
  __AudioCtx.prototype.createGain=function(){
    return {gain:{value:1},connect:function(n){return n;},disconnect:function(){}};
  };
  __AudioCtx.prototype.createBuffer=function(c,l,r){return __makeAudioBuffer(c,l,r);};
  __AudioCtx.prototype.createBufferSource=function(){
    return {buffer:null,loop:false,connect:function(n){return n;},
      disconnect:function(){},start:function(){},stop:function(){},onended:null};
  };
  __AudioCtx.prototype.createScriptProcessor=function(){
    return {connect:function(n){return n;},disconnect:function(){},onaudioprocess:null};
  };
  __AudioCtx.prototype.close=function(){this.state="closed";return Promise.resolve();};
  __AudioCtx.prototype.resume=function(){return Promise.resolve();};
  __AudioCtx.prototype.suspend=function(){return Promise.resolve();};
  function __OfflineCtx(ch,len,rate){
    __AudioCtx.call(this);
    this.length=Math.max(1,len|0);this.sampleRate=rate||44100;this.__ch=Math.max(1,ch|0);
    this.oncomplete=null;
  }
  __OfflineCtx.prototype=Object.create(__AudioCtx.prototype);
  __OfflineCtx.prototype.startRendering=function(){
    var buf=__makeAudioBuffer(this.__ch,this.length,this.sampleRate);
    if(typeof this.oncomplete==="function")this.oncomplete({renderedBuffer:buf});
    return Promise.resolve(buf);
  };
  globalThis.AudioContext=__AudioCtx;
  globalThis.webkitAudioContext=__AudioCtx;
  globalThis.OfflineAudioContext=__OfflineCtx;
  globalThis.webkitOfflineAudioContext=__OfflineCtx;

  // The DOM model (installed after this prologue) calls attachCanvas for every
  // <canvas> element it creates; measureText backs its 2D contexts.
  globalThis.__cerberusFarble={
    attachCanvas:__attachCanvas,
    measureText:__measure
  };
"##;

/// SplitMix64 — a small, fast, well-distributed finalizer. Used only for
/// fingerprint noise, never for anything security-sensitive.
fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perturbation_is_deterministic() {
        let f = SeededFarbling::new(0xABCD);
        for i in 0..64 {
            assert_eq!(
                f.perturb(Channel::Canvas, i, 128),
                f.perturb(Channel::Canvas, i, 128)
            );
        }
    }

    #[test]
    fn perturbation_is_bounded_so_output_still_renders() {
        let f = SeededFarbling::new(7);
        for v in 0u8..=255 {
            for i in 0..16 {
                let out = f.perturb(Channel::Canvas, i, v);
                assert!(out.abs_diff(v) <= 1, "delta too large at v={v}, i={i}");
            }
        }
    }

    #[test]
    fn two_heads_do_not_correlate() {
        let a = SeededFarbling::new(1);
        let b = SeededFarbling::new(2);
        let differing = (0..1024u64)
            .filter(|&i| a.perturb(Channel::Canvas, i, 128) != b.perturb(Channel::Canvas, i, 128))
            .count();
        // Distinct seeds must diverge across the surface (not be near-identical).
        assert!(differing > 256, "only {differing}/1024 samples differed");
    }
}
