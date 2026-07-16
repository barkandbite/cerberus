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
        // Export the per-head seed onto the global object so the DOM prelude's
        // crypto.getRandomValues/randomUUID shim (guard: `typeof g.__FARBLE_HI
        // === "number"`) reads THIS head's seed. `var __FARBLE_HI/_LO` alone are
        // IIFE-local, so without this every head fell back to one shared crypto
        // stream — a cross-head correlation tell. The values are u32 (< 2^53, so
        // JS `typeof === "number"`) and per-head (derived from `seed`).
        format!("(function(){{var __FARBLE_HI={hi},__FARBLE_LO={lo};\nglobalThis.__FARBLE_HI=__FARBLE_HI;globalThis.__FARBLE_LO=__FARBLE_LO;\n{FARBLING_SHIMS}\n}})();\n")
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
/// - **WebGL**: identity strings (`VENDOR`/`RENDERER`/`VERSION`, unmasked
///   vendor/renderer, limits, extensions) report one fixed, coherent
///   Chrome-142-on-Windows-11 Intel/ANGLE/D3D11 persona — the same for every
///   head, so the GPU identity looks like a real browser rather than an obvious
///   "Cerberus" tell. `readPixels` — the actual entropy surface — still returns
///   per-head seeded noise, so heads don't correlate on the pixel hash.
/// - **audio**: analyser/offline-render readbacks return near-silence with
///   per-head noise in the low bits, deterministic per head.
/// - **font metrics + enumeration**: `measureText` resolves the requested family
///   against this head's presented font set (`__CERBERUS_PROFILE__.fonts`). An
///   *installed* family (a generic, or a name in the set) gets a stable
///   per-(head, family) advance; a *non-installed* family measures identically to
///   the generic fallback, so the classic width-comparison enumeration trick sees
///   it as absent. This makes `measureText` and `document.fonts.check` agree on
///   one per-head-random font list rather than exposing that only bundled faces
///   exist (a headless tell).
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

  // Math.random, seeded from this head's farble seed (mulberry32). QuickJS's
  // default Math.random is seeded from process entropy, so it (a) varies every
  // run — a non-determinism that makes a page's script-driven layout render
  // differently each load, breaking reproducibility — and (b) is an uncorrelated
  // per-process entropy source, a fingerprinting tell. A seeded generator makes
  // renders reproducible and gives each head one coherent, stable random stream.
  (function(){
    var __mr=(__FARBLE_LO ^ Math.imul(__FARBLE_HI,0x9E3779B9))>>>0;
    Math.random=function(){
      __mr=(__mr+0x6D2B79F5)>>>0;
      var t=__mr;
      t=Math.imul(t^(t>>>15), t|1)>>>0;
      t=(t+Math.imul(t^(t>>>7), t|61))>>>0;
      return ((t^(t>>>14))>>>0)/4294967296;
    };
  })();

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

  // ---- font metrics + enumeration defense ----
  // The CSS generic families (and system aliases) always resolve, so they are
  // "installed" for measurement purposes.
  var __GENERIC={"serif":1,"sans-serif":1,"monospace":1,"cursive":1,"fantasy":1,
    "system-ui":1,"ui-serif":1,"ui-sans-serif":1,"ui-monospace":1,"ui-rounded":1,
    "math":1,"emoji":1,"-apple-system":1,"blinkmacsystemfont":1};
  // The first family named in a CSS `font` shorthand (or bare family), unquoted
  // and lowercased.
  function __family(font){
    var s=String(font);
    var m=/(?:\d*\.?\d+)(?:px|pt|pc|em|rem|ex|ch|vw|vh|%)\s+(.+)$/.exec(s);
    var fam=(m?m[1]:s).split(",")[0].trim().replace(/^["']|["']$/g,"");
    return fam.toLowerCase();
  }
  // Whether the head presents `fam` as installed: a generic family, or a name in
  // this head's per-head font set (globalThis.__CERBERUS_PROFILE__.fonts). The
  // measureText enumeration trick and document.fonts.check both consult this same
  // list, so they agree.
  function __fontInstalled(fam){
    if(__GENERIC[fam])return true;
    var p=globalThis.__CERBERUS_PROFILE__;
    var list=(p&&p.fonts)||null;
    if(!list)return false;
    for(var i=0;i<list.length;i++){if(String(list[i]).toLowerCase()===fam)return true;}
    return false;
  }
  globalThis.__cerbFontInstalled=__fontInstalled;
  function __measure(t,font){
    var px=parseFloat(font)||10;
    var fam=__family(font);
    // A non-installed family renders in the generic fallback, so it must measure
    // IDENTICALLY to that fallback — otherwise the classic width-comparison trick
    // detects it. Installed families get a stable per-(head, family) advance so
    // real installed fonts still differ from each other and from the fallback.
    var key=__fontInstalled(fam)?fam:"sans-serif";
    var adv=0;for(var i=0;i<t.length;i++){adv+=(t.charCodeAt(i)===32)?0.33:0.6;}
    var r=__rng(4,"fam|"+key);
    var factor=0.90+((r()%2000)/2000)*0.22; // ~[0.90,1.12], stable per head+family
    var width=adv*px*factor;
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
  function __makeWebGL(el,isV2){
    var cacheKey=isV2?"__cerbGL2":"__cerbGL";
    if(el[cacheKey])return el[cacheKey];
    var wh=__dims(el);
    var gl={canvas:el,drawingBufferWidth:wh[0],drawingBufferHeight:wh[1],
      // ---- GL enum constants, so gl.getParameter(gl.VENDOR) resolves (a bare
      // gl.VENDOR would be undefined → getParameter(undefined) → default 0). ----
      VENDOR:0x1F00,RENDERER:0x1F01,VERSION:0x1F02,SHADING_LANGUAGE_VERSION:0x8B8C,
      UNMASKED_VENDOR_WEBGL:0x9245,UNMASKED_RENDERER_WEBGL:0x9246,
      MAX_TEXTURE_SIZE:0x0D33,MAX_CUBE_MAP_TEXTURE_SIZE:0x851C,MAX_RENDERBUFFER_SIZE:0x84E8,
      MAX_VIEWPORT_DIMS:0x0D3A,MAX_VERTEX_ATTRIBS:0x8869,MAX_VERTEX_UNIFORM_VECTORS:0x8DFB,
      MAX_VARYING_VECTORS:0x8DFC,MAX_COMBINED_TEXTURE_IMAGE_UNITS:0x8B4D,
      MAX_VERTEX_TEXTURE_IMAGE_UNITS:0x8B4C,MAX_TEXTURE_IMAGE_UNITS:0x8872,
      MAX_FRAGMENT_UNIFORM_VECTORS:0x8DFD,ALIASED_LINE_WIDTH_RANGE:0x846E,
      ALIASED_POINT_SIZE_RANGE:0x846D,MAX_TEXTURE_MAX_ANISOTROPY_EXT:0x84FF,
      RED_BITS:0x0D52,GREEN_BITS:0x0D53,BLUE_BITS:0x0D54,ALPHA_BITS:0x0D55,
      DEPTH_BITS:0x0D56,STENCIL_BITS:0x0D57,SAMPLES:0x80A9,SAMPLE_BUFFERS:0x80A8,
      ARRAY_BUFFER:0x8892,ELEMENT_ARRAY_BUFFER:0x8893,STATIC_DRAW:0x88E4,
      FLOAT:0x1406,TRIANGLES:0x0004,COLOR_BUFFER_BIT:0x4000,DEPTH_BUFFER_BIT:0x0100,
      VERTEX_SHADER:0x8B31,FRAGMENT_SHADER:0x8B30,COMPILE_STATUS:0x8B81,LINK_STATUS:0x8B82,
      HIGH_FLOAT:0x8DF2,MEDIUM_FLOAT:0x8DF1,LOW_FLOAT:0x8DF0,
      HIGH_INT:0x8DF5,MEDIUM_INT:0x8DF4,LOW_INT:0x8DF3,
      getParameter:function(p){
        // GPU identity comes from the coherent per-window profile when present,
        // read LAZILY here (page scripts run after all prologues, so prologue
        // injection order does not matter). Falls through to the fixed persona
        // below when no profile (or no .gpu) is injected.
        var __q=p|0, __pg=globalThis.__CERBERUS_PROFILE__; __pg=__pg&&__pg.gpu;
        if(__pg){
          if(__q===0x1F00&&typeof __pg.vendor==="string")return __pg.vendor;
          if(__q===0x1F01&&typeof __pg.renderer==="string")return __pg.renderer;
          if(__q===0x9245&&typeof __pg.unmaskedVendor==="string")return __pg.unmaskedVendor;
          if(__q===0x9246&&typeof __pg.unmaskedRenderer==="string")return __pg.unmaskedRenderer;
        }
        if(isV2)switch(p|0){
          // Core WebGL2 limits (real ANGLE/Intel WebGL2). Only reached on a
          // webgl2 context; a webgl1 context never answers these enums.
          case 0x8073: return 2048;       // MAX_3D_TEXTURE_SIZE
          case 0x88FF: return 2048;       // MAX_ARRAY_TEXTURE_LAYERS
          case 0x8CDF: return 8;          // MAX_COLOR_ATTACHMENTS
          case 0x8824: return 8;          // MAX_DRAW_BUFFERS
          case 0x8D57: return 8;          // MAX_SAMPLES
          case 0x8A2F: return 72;         // MAX_UNIFORM_BUFFER_BINDINGS
          case 0x8A2B: return 15;         // MAX_VERTEX_UNIFORM_BLOCKS
          case 0x8A2D: return 15;         // MAX_FRAGMENT_UNIFORM_BLOCKS
          case 0x8D6B: return 4294967294; // MAX_ELEMENT_INDEX
          case 0x80E9: return 150000;     // MAX_ELEMENTS_INDICES
          case 0x80E8: return 1048575;    // MAX_ELEMENTS_VERTICES
          case 0x84FD: return 2;          // MAX_TEXTURE_LOD_BIAS
          case 0x8B4B: return 60;         // MAX_VARYING_COMPONENTS
          case 0x8C8A: return 4;          // MAX_TRANSFORM_FEEDBACK_SEPARATE_ATTRIBS
          case 0x8A30: return 65536;      // MAX_UNIFORM_BLOCK_SIZE
          case 0x8A34: return 256;        // UNIFORM_BUFFER_OFFSET_ALIGNMENT
          case 0x8A2E: return 24;         // MAX_COMBINED_UNIFORM_BLOCKS
        }
        switch(p|0){
          // Coherent Chrome-142-on-Windows-11 Intel/ANGLE/D3D11 persona (fixed).
          case 0x1F00: return "WebKit";
          case 0x1F01: return "WebKit WebGL";
          case 0x1F02: return isV2?"WebGL 2.0 (OpenGL ES 3.0 Chromium)":"WebGL 1.0 (OpenGL ES 2.0 Chromium)";
          case 0x8B8C: return isV2?"WebGL GLSL ES 3.00 (OpenGL ES GLSL ES 3.0 Chromium)":"WebGL GLSL ES 1.0 (OpenGL ES GLSL ES 1.0 Chromium)";
          case 0x9245: return "Google Inc. (Intel)";
          case 0x9246: return "ANGLE (Intel, Intel(R) UHD Graphics 630 (0x00003E9B) Direct3D11 vs_5_0 ps_5_0, D3D11)";
          case 0x0D33: return 16384; // MAX_TEXTURE_SIZE
          case 0x851C: return 16384; // MAX_CUBE_MAP_TEXTURE_SIZE
          case 0x84E8: return 16384; // MAX_RENDERBUFFER_SIZE
          case 0x0D3A: return new Int32Array([32767,32767]); // MAX_VIEWPORT_DIMS
          case 0x8869: return 16;    // MAX_VERTEX_ATTRIBS
          case 0x8DFB: return 4096;  // MAX_VERTEX_UNIFORM_VECTORS
          case 0x8DFC: return 30;    // MAX_VARYING_VECTORS
          case 0x8B4D: return 32;    // MAX_COMBINED_TEXTURE_IMAGE_UNITS
          case 0x8B4C: return 16;    // MAX_VERTEX_TEXTURE_IMAGE_UNITS
          case 0x8872: return 16;    // MAX_TEXTURE_IMAGE_UNITS
          case 0x8DFD: return 1024;  // MAX_FRAGMENT_UNIFORM_VECTORS
          case 0x846E: return new Float32Array([1,1]);    // ALIASED_LINE_WIDTH_RANGE
          case 0x846D: return new Float32Array([1,1024]); // ALIASED_POINT_SIZE_RANGE
          case 0x84FF: return 16;    // MAX_TEXTURE_MAX_ANISOTROPY_EXT
          case 0x0D52: return 8;     // RED_BITS
          case 0x0D53: return 8;     // GREEN_BITS
          case 0x0D54: return 8;     // BLUE_BITS
          case 0x0D55: return 8;     // ALPHA_BITS
          case 0x0D56: return 24;    // DEPTH_BITS
          case 0x0D57: return 0;     // STENCIL_BITS
          case 0x80A9: return 0;     // SAMPLES
          case 0x80A8: return 0;     // SAMPLE_BUFFERS
          default: return 0;
        }
      },
      getSupportedExtensions:function(){return isV2?["EXT_color_buffer_float","EXT_color_buffer_half_float","EXT_disjoint_timer_query_webgl2","EXT_float_blend","EXT_texture_compression_bptc","EXT_texture_compression_rgtc","EXT_texture_filter_anisotropic","EXT_texture_norm16","KHR_parallel_shader_compile","OES_draw_buffers_indexed","OES_texture_float_linear","OVR_multiview2","WEBGL_clip_cull_distance","WEBGL_compressed_texture_s3tc","WEBGL_compressed_texture_s3tc_srgb","WEBGL_debug_renderer_info","WEBGL_debug_shaders","WEBGL_lose_context","WEBGL_multi_draw","WEBGL_provoking_vertex"]:["ANGLE_instanced_arrays","EXT_blend_minmax","EXT_color_buffer_half_float","EXT_disjoint_timer_query","EXT_float_blend","EXT_frag_depth","EXT_shader_texture_lod","EXT_texture_compression_bptc","EXT_texture_compression_rgtc","EXT_texture_filter_anisotropic","EXT_sRGB","OES_element_index_uint","OES_fbo_render_mipmap","OES_standard_derivatives","OES_texture_float","OES_texture_float_linear","OES_texture_half_float","OES_texture_half_float_linear","OES_vertex_array_object","WEBGL_color_buffer_float","WEBGL_compressed_texture_s3tc","WEBGL_compressed_texture_s3tc_srgb","WEBGL_debug_renderer_info","WEBGL_debug_shaders","WEBGL_depth_texture","WEBGL_draw_buffers","WEBGL_lose_context","WEBGL_multi_draw"];},
      getExtension:function(name){
        if(name==="WEBGL_debug_renderer_info")return {UNMASKED_VENDOR_WEBGL:0x9245,UNMASKED_RENDERER_WEBGL:0x9246};
        if(name==="EXT_texture_filter_anisotropic")return {MAX_TEXTURE_MAX_ANISOTROPY_EXT:0x84FF,TEXTURE_MAX_ANISOTROPY_EXT:0x84FE};
        if(name==="OES_vertex_array_object")return {createVertexArrayOES:function(){return {};},bindVertexArrayOES:function(){},deleteVertexArrayOES:function(){},VERTEX_ARRAY_BINDING_OES:0x85B5};
        if(name==="ANGLE_instanced_arrays")return {drawArraysInstancedANGLE:function(){},drawElementsInstancedANGLE:function(){},vertexAttribDivisorANGLE:function(){},VERTEX_ATTRIB_ARRAY_DIVISOR_ANGLE:0x88FE};
        if(name==="WEBGL_lose_context")return {loseContext:function(){},restoreContext:function(){}};
        return null;
      },
      readPixels:function(x,y,w,h,fmt,type,out){
        if(out&&out.length){var r=__rng(3,"rp|"+x+","+y+","+w+"x"+h);
          for(var i=0;i<out.length;i++)out[i]=r()&255;}
      },
      getContextAttributes:function(){return {alpha:true,antialias:true,depth:true,desynchronized:false,failIfMajorPerformanceCaveat:false,powerPreference:"default",premultipliedAlpha:true,preserveDrawingBuffer:false,stencil:false,xrCompatible:false};},
      getShaderPrecisionFormat:function(shaderType,precisionType){
        var pt=precisionType|0;
        // Integer precisions report a different profile from floats.
        if(pt===0x8DF5||pt===0x8DF4||pt===0x8DF3)return {rangeMin:31,rangeMax:30,precision:0};
        return {rangeMin:127,rangeMax:127,precision:23};
      },
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
    if(isV2){
      // Core WebGL2 method surface — added only to the webgl2 context so the
      // webgl1 surface stays byte-identical. Plausible no-op / shaped returns.
      gl.createVertexArray=function(){return {};};gl.deleteVertexArray=function(){};
      gl.bindVertexArray=function(){};gl.isVertexArray=function(){return false;};
      gl.createQuery=function(){return {};};gl.deleteQuery=function(){};
      gl.beginQuery=function(){};gl.endQuery=function(){};
      gl.getQuery=function(){return null;};gl.getQueryParameter=function(){return 0;};
      gl.createSampler=function(){return {};};gl.deleteSampler=function(){};
      gl.bindSampler=function(){};gl.samplerParameteri=function(){};
      gl.createTransformFeedback=function(){return {};};gl.bindTransformFeedback=function(){};
      gl.beginTransformFeedback=function(){};gl.endTransformFeedback=function(){};
      gl.transformFeedbackVaryings=function(){};gl.texImage3D=function(){};
      gl.texStorage2D=function(){};gl.texStorage3D=function(){};gl.texSubImage3D=function(){};
      gl.getBufferSubData=function(){};gl.drawArraysInstanced=function(){};
      gl.drawElementsInstanced=function(){};gl.vertexAttribDivisor=function(){};
      gl.drawBuffers=function(){};gl.clearBufferfv=function(){};
      gl.clearBufferiv=function(){};gl.clearBufferuiv=function(){};
      gl.getUniformBlockIndex=function(){return 0;};gl.uniformBlockBinding=function(){};
      gl.bindBufferBase=function(){};gl.bindBufferRange=function(){};
      gl.fenceSync=function(){return {};};gl.deleteSync=function(){};
      gl.clientWaitSync=function(){return 0;};gl.getFragDataLocation=function(){return -1;};
      gl.getActiveUniformBlockParameter=function(){return 0;};gl.getActiveUniforms=function(){return [];};
      gl.invalidateFramebuffer=function(){};gl.readBuffer=function(){};
      gl.renderbufferStorageMultisample=function(){};gl.blitFramebuffer=function(){};
    }
    el[cacheKey]=gl;return gl;
  }
  function __attachCanvas(el){
    if(el.__cerbCanvas)return el;
    el.__cerbCanvas=true;
    if(el.width===undefined)el.width=300;
    if(el.height===undefined)el.height=150;
    el.getContext=function(kind){
      kind=String(kind||"2d").toLowerCase();
      if(kind==="webgl2"||kind==="experimental-webgl2")return __makeWebGL(el,true);
      if(kind.indexOf("webgl")===0||kind==="experimental-webgl")return __makeWebGL(el,false);
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

    #[test]
    fn math_random_is_overridden_with_the_seeded_prng() {
        // Math.random must be replaced by a generator keyed off the per-head
        // farble seed, not QuickJS's process-entropy default — otherwise the same
        // script-driven page renders differently each load (non-reproducible) and
        // Math.random leaks per-process entropy as a fingerprint tell.
        assert!(FARBLING_SHIMS.contains("Math.random=function"));
        assert!(
            FARBLING_SHIMS.contains("__FARBLE_LO ^ Math.imul(__FARBLE_HI"),
            "the Math.random seed must derive from this head's farble seed"
        );
    }

    #[test]
    fn webgl_reports_coherent_chrome_intel_persona() {
        // Identity strings must be a single, fixed, real Chrome-on-Windows-Intel
        // persona — no "Cerberus" GPU tell that scanners flag instantly.
        assert!(FARBLING_SHIMS.contains(r#"case 0x1F00: return "WebKit";"#));
        assert!(FARBLING_SHIMS.contains(r#"case 0x1F01: return "WebKit WebGL";"#));
        assert!(FARBLING_SHIMS.contains(r#"case 0x9245: return "Google Inc. (Intel)";"#));
        assert!(FARBLING_SHIMS.contains(
            "ANGLE (Intel, Intel(R) UHD Graphics 630 (0x00003E9B) Direct3D11 vs_5_0 ps_5_0, D3D11)"
        ));
        // WebGL1 and WebGL2 VERSION / GLSL strings.
        assert!(FARBLING_SHIMS.contains("WebGL 1.0 (OpenGL ES 2.0 Chromium)"));
        assert!(FARBLING_SHIMS.contains("WebGL 2.0 (OpenGL ES 3.0 Chromium)"));
        assert!(FARBLING_SHIMS.contains("WebGL GLSL ES 1.0 (OpenGL ES GLSL ES 1.0 Chromium)"));
        assert!(FARBLING_SHIMS.contains("WebGL GLSL ES 3.00 (OpenGL ES GLSL ES 3.0 Chromium)"));

        // The old "Cerberus" GPU tells must be gone from the WebGL identity.
        assert!(!FARBLING_SHIMS.contains("Software Renderer"));
        assert!(!FARBLING_SHIMS.contains("(Cerberus)"));

        // Real numeric getParameter values (not the old all-zero / 4096 stubs).
        assert!(FARBLING_SHIMS.contains("case 0x0D33: return 16384;")); // MAX_TEXTURE_SIZE
        assert!(FARBLING_SHIMS.contains("case 0x8869: return 16;")); // MAX_VERTEX_ATTRIBS
        assert!(FARBLING_SHIMS.contains("case 0x8DFC: return 30;")); // MAX_VARYING_VECTORS
        assert!(FARBLING_SHIMS.contains("new Int32Array([32767,32767])")); // MAX_VIEWPORT_DIMS

        // GL enum constants exposed on the object so gl.getParameter(gl.VENDOR) resolves.
        assert!(FARBLING_SHIMS.contains("VENDOR:0x1F00"));
        assert!(FARBLING_SHIMS.contains("UNMASKED_RENDERER_WEBGL:0x9246"));
    }

    #[test]
    fn webgl_readpixels_noise_stays_per_head() {
        // The identity strings are now fixed/coherent, but the readPixels entropy
        // surface must still diverge across heads (readPixels uses the
        // WebglReadPixels channel). Two distinct seeds must not correlate.
        let a = SeededFarbling::new(1);
        let b = SeededFarbling::new(2);
        let differing = (0..1024u64)
            .filter(|&i| {
                a.perturb(Channel::WebglReadPixels, i, 128)
                    != b.perturb(Channel::WebglReadPixels, i, 128)
            })
            .count();
        assert!(
            differing > 256,
            "only {differing}/1024 readPixels samples differed"
        );

        // The per-head seed is threaded into the JS shim, so two heads install
        // distinct prologues (distinct __FARBLE_HI/__FARBLE_LO) despite sharing
        // the identical fixed GPU identity strings.
        assert_ne!(a.js_prologue(), b.js_prologue());
    }
}
