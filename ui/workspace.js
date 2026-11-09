let workspaceStarted=false;
window.addEventListener("rove:workspace", () => {
    if(workspaceStarted)return;workspaceStarted=true;
    'use strict';
    const $=id=>document.getElementById("game-"+id),key=document.querySelector('meta[name="console-key"]').content;
    const client=Array.from(crypto.getRandomValues(new Uint8Array(16)),x=>x.toString(16).padStart(2,'0')).join('');
    let renderer,connected=false,stopped=false,moving=false,moveBusy=false,moveSequence=0,resizeBusy=false;
    let moveQueued=false,releaseQueued=false;
    let lastSize='',lastPlayers='',lastTick=0,resizeRetry=0,lastSample=null,hasFrame=false;
    let runtimeIdentity=null;
    const held=new Set(),nodes=new Map();
    async function post(path,data,keepalive=false) {
        const timeout=path==='/api/runtime/move'?1000:path==='/api/runtime/render-asset'?20000:12000;
        const response=await fetch(path,{method:'POST',headers:{'Content-Type':'application/json','X-Console-Key':key},body:JSON.stringify(data),signal:AbortSignal.timeout(timeout),keepalive});
        const result=await response.json();if(!response.ok||!result.ok)throw new Error(result.error||'Session request failed');return result;
    }
    let fontGeneration=0;
    const fonts=new RoveFonts.FontCache((kind,reference,weight,style)=>post('/api/runtime/render-asset',{kind,reference,weight,style}),()=>{
        ++fontGeneration;
        for(const entry of nodes.values()){fonts.apply(entry.element,entry.font);if(entry.textFrame)RoveText.apply(entry,entry.textFrame,fonts,fontGeneration);}
    });
    try{renderer=RoveWorld.mount($('surface'),{
        forceSoftware:new URLSearchParams(location.search).get('renderer')==='software',
        loadAsset:(kind,reference)=>post('/api/runtime/render-asset',{kind,reference}),
        onBackendChange:backend=>{$('statistics').textContent=backend==='software'?'Software renderer · waiting for session':'GPU renderer · waiting for session';}
    });}catch(error){$('waiting-text').textContent='The game display could not start: '+error.message;return;}
    const controls=createKeyboardControls({surface:$('surface'),label:$('controls-state'),key,onResult:result=>{
        if(!result.ok)$('action-state').textContent=result.error;
    }});
    const scrolling=RoveGuiScroll.create({surface:$('game-gui'),
        enabled:()=>connected&&!document.hidden&&!document.getElementById('workspace-page').hidden,
        send:action=>post('/api/runtime/gui',action),onError:error=>{$('action-state').textContent=error.message;}});
    function resetMovement(){held.clear();if(moving||moveBusy)void sendMovement(true);}
    async function sendMovement(force=false) {
        if(!connected||(stopped&&!force))return;
        if(moveBusy){moveQueued=true;releaseQueued||=force;return;}
        const active=!stopped&&document.activeElement===$('surface')&&document.visibilityState==='visible';
        const x=active?Number(held.has('KeyD'))-Number(held.has('KeyA')):0;
        const z=active?Number(held.has('KeyS'))-Number(held.has('KeyW')):0;
        if(!x&&!z&&!moving&&!force)return;
        const identity=runtimeIdentity,direction=x||z?renderer.direction(x,z):[0,0,0];
        moveBusy=true;
        try{
            await post('/api/runtime/move',{client,sequence:++moveSequence,direction},force&&!x&&!z);
            if(identity===runtimeIdentity)moving=Boolean(x||z);
        }catch(error){
            if(identity===runtimeIdentity){
                // A failed response can still leave an earlier move active.
                moving=moving||Boolean(x||z);
                $('action-state').textContent=error.message;
            }
        }finally{
            moveBusy=false;
            if(moveQueued){
                const release=releaseQueued;
                moveQueued=releaseQueued=false;
                void sendMovement(release);
            }
        }
    }
    document.addEventListener('keydown',event=>{
        if(!event.isTrusted||!connected||document.activeElement!==$('surface'))return;
        if(event.code==='Escape'){resetMovement();return;}
        if(['KeyW','KeyA','KeyS','KeyD'].includes(event.code)&&!held.has(event.code)&&!event.repeat){held.add(event.code);void sendMovement();}
        if(event.code==='Space'&&!event.repeat)void post('/api/runtime/jump',{action:'jump'}).catch(error=>{$('action-state').textContent=error.message;});
    });
    document.addEventListener('keyup',event=>{if(held.delete(event.code))void sendMovement();});
    $('surface').addEventListener('blur',resetMovement);window.addEventListener('blur',resetMovement);
    document.addEventListener('visibilitychange',()=>{if(document.hidden){resetMovement();scrolling.reset();}});
    setInterval(()=>{if(!moveBusy)void sendMovement();},200);
    function requestMessage(error) {
        return ['TimeoutError','AbortError'].includes(error?.name)?'The client is busy. Waiting for the next game frame…':error.message;
    }
    async function syncSize() {
        const size=[Math.round($('surface').clientWidth),Math.round($('surface').clientHeight)],serialized=JSON.stringify(size);
        if(resizeBusy||serialized===lastSize||Date.now()<resizeRetry||size.some(n=>n<=0))return;
        resizeBusy=true;const identity=runtimeIdentity;
        try{await post('/api/runtime/display',{size});if(identity===runtimeIdentity){lastSize=serialized;resizeRetry=0;}}
        catch(error){if(identity===runtimeIdentity)resizeRetry=Date.now()+2500;}
        finally{resizeBusy=false;}
    }
    const rgb=(value,alpha=1)=>{const c=value?.$color3;return Array.isArray(c)&&c.length===3?'rgba('+c.map(x=>Math.round(Math.max(0,Math.min(1,Number(x)||0))*255)).join(',')+','+alpha+')':'transparent';};
    const finite=(x,fallback=0)=>typeof x==='number'&&Number.isFinite(x)?x:fallback;
    async function guiAction(data){try{await post('/api/runtime/gui',data);return true;}catch(error){$('action-state').textContent=error.message;return false;}}
    const reportImageLoaded=action=>post('/api/runtime/gui',action).then(()=>true,()=>false);
    const freshTextFocus=()=>({revision:-1,pending:0,tail:Promise.resolve(),mirroring:false});
    let textFocus=freshTextFocus();
    let textEncoder,textBuffer;
    function queueTextAction(entry,action){
        const state=textFocus;
        state.pending++;
        state.tail=state.tail.then(async()=>{
            if(state!==textFocus)return;
            const data={...action,id:entry.element.dataset.instance};
            if(entry.pendingEdit===action)entry.pendingEdit=null;
            if(data.kind!=='text_focus'){
                if(entry.focusRevision===undefined)return;
                data.revision=entry.focusRevision;
            }else entry.focusRevision=undefined;
            const {textFocus:focus}=await post('/api/runtime/gui',data);
            if(state!==textFocus||!focus)return;
            state.revision=Math.max(state.revision,focus.revision);
            if(data.kind==='text_focus'&&focus.id===data.id)entry.focusRevision=focus.revision;
        }).catch(error=>{
            if(state===textFocus)$('action-state').textContent=error.message;
        }).finally(()=>state.pending--);
    }
    function bindTextBox(entry){
        const e=entry.element;
        let submitted=false,composing=false;
        e.addEventListener('focus',()=>{
            submitted=false;
            if(textFocus.revision<0||textFocus.mirroring)return;
            const clear=entry.textFrame?.clearTextOnFocus&&!e.readOnly;
            if(clear)e.value='';
            entry.editorSource=clear?'':String(entry.textFrame?.text??e.value);
            entry.editorRevision=textFocus.selection?0:undefined;
            entry.lastSelection=null;
            queueTextAction(entry,{kind:'text_focus'});
        });
        e.addEventListener('blur',()=>{
            entry.pendingEdit=null;
            composing=false;
            if(textFocus.revision>=0&&!textFocus.mirroring)
                queueTextAction(entry,{kind:'text_blur',submitted});
        });
        e.addEventListener('change',()=>{
            if(textFocus.revision<0&&!textFocus.mirroring)void guiAction({id:e.dataset.instance,kind:'text',text:e.value.slice(0,4096)});
        });
        const edit=event=>{
            if(event.isComposing||composing)return;
            if(!entry.multiLine&&/[\r\n]/.test(e.value))e.value=e.value.replace(/[\r\n]+/g,' ');
            if(textFocus.revision<0||e.readOnly)return;
            textEncoder??=new TextEncoder();
            textBuffer??=new Uint8Array(4096);
            const {read}=textEncoder.encodeInto(e.value,textBuffer);
            if(read<e.value.length)e.value=e.value.slice(0,read);
            entry.editorSource=e.value;
            queueEditorChange(entry,{kind:'text_edit',text:e.value,...readTextSelection(entry)});
        };
        e.addEventListener('input',edit);
        e.addEventListener('compositionstart',()=>{composing=true;});
        e.addEventListener('compositionend',event=>{composing=false;edit(event);});
        e.addEventListener('selectionchange',()=>{
            if(document.activeElement!==e||composing||textFocus.mirroring||entry.editorRevision===undefined)return;
            const previous=entry.lastSelection,selection=readTextSelection(entry);
            if(!previous||previous.cursor!==selection.cursor||previous.anchor!==selection.anchor)
                queueEditorChange(entry,selection);
        });
        e.addEventListener('keydown',event=>{
            if(event.key==='Enter'&&!entry.multiLine&&!event.isComposing){
                submitted=true;event.preventDefault();e.blur();
            }
        });
    }
    function queueEditorChange(entry,changes){
        if(entry.pendingEdit)Object.assign(entry.pendingEdit,changes);
        else {
            entry.pendingEdit={kind:'text_select',...changes};
            queueTextAction(entry,entry.pendingEdit);
        }
    }
    function readTextSelection(entry){
        if(entry.editorRevision===undefined)return {};
        const e=entry.element,backward=e.selectionDirection==='backward';
        const start=RoveText.editorOffset(entry.editorSource,e.selectionStart,true)+1;
        const end=RoveText.editorOffset(entry.editorSource,e.selectionEnd,true)+1;
        const selection={editorRevision:entry.editorRevision,cursor:backward?start:end,anchor:start===end?-1:backward?end:start};
        entry.lastSelection=selection;
        return selection;
    }
    function applyTextFocus(focus,editor){
        if(!focus||textFocus.pending||focus.revision<textFocus.revision)return;
        textFocus.revision=focus.revision;
        textFocus.selection=!!editor;
        const entry=nodes.get(focus.id),target=entry?.element,active=document.activeElement;
        textFocus.mirroring=true;
        try{
            if(active?.matches?.('textarea.game-node')&&active!==target)active.blur();
            if(target&&!target.inert){
                const changedFocus=entry.focusRevision!==focus.revision||active!==target;
                entry.focusRevision=focus.revision;
                if(active!==target)target.focus({preventScroll:true});
                if(editor&&(changedFocus||entry.editorRevision!==editor.revision)){
                    entry.editorSource=String(entry.textFrame?.text??'');
                    entry.editorRevision=editor.revision;
                    target.value=entry.editorSource;
                    const cursor=RoveText.editorOffset(entry.editorSource,editor.cursor-1,false);
                    const anchor=editor.anchor<0?cursor:RoveText.editorOffset(entry.editorSource,editor.anchor-1,false);
                    target.setSelectionRange(Math.min(cursor,anchor),Math.max(cursor,anchor),cursor<anchor?'backward':'forward');
                    readTextSelection(entry);
                }
            }
        }finally{textFocus.mirroring=false;}
    }
    const svgNamespace='http://www.w3.org/2000/svg';
    let nextGradient=0;
    function svgElement(tag,attributes={}){
        const element=document.createElementNS(svgNamespace,tag);
        for(const [name,value] of Object.entries(attributes))element.setAttribute(name,String(value));
        return element;
    }
    function syncGuiLayer(layer,owner){
        for(const property of ['left','top','width','height','transform','zIndex','clipPath','visibility'])
            layer.style[property]=owner.style[property];
    }
    function gradientSample(stops,time){
        const next=stops.findIndex(stop=>stop[0]>=time);
        const first=stops[next];
        if(first[0]===time){
            let last=next;
            while(stops[last+1]?.[0]===time)last++;
            return [first.slice(1),stops[last].slice(1)];
        }
        const previous=stops[next-1],fraction=(time-previous[0])/(first[0]-previous[0]);
        const value=previous.slice(1).map((component,i)=>component+(first[i+1]-component)*fraction);
        return [value,value];
    }
    function gradientPaint(layer,gradient,width,height,tint,opacity){
        const x=width*(.5+gradient.offset[0]),y=height*(.5+gradient.offset[1]);
        const id='rove-gradient-'+(++nextGradient);
        const attributes={id,gradientUnits:'userSpaceOnUse',
            spreadMethod:['pad','repeat','reflect'][gradient.tile]||'pad','color-interpolation':'sRGB'};
        let paint;
        if(gradient.type===1){
            paint=svgElement('radialGradient',{...attributes,cx:x,cy:y,r:(width+height)*gradient.scale/4});
        }else if(gradient.type===3){
            const rx=width*gradient.scale/2,ry=height*gradient.scale/2;
            paint=svgElement('radialGradient',{...attributes,cx:0,cy:0,r:1,
                gradientTransform:`translate(${x} ${y}) rotate(${gradient.rotation}) scale(${rx} ${ry})`});
        }else{
            const angle=gradient.rotation*Math.PI/180,dx=Math.cos(angle),dy=Math.sin(angle);
            const half=Math.min(width/(2*Math.abs(dx)),height/(2*Math.abs(dy)))*gradient.scale;
            paint=svgElement('linearGradient',{...attributes,x1:x-dx*half,y1:y-dy*half,x2:x+dx*half,y2:y+dy*half});
        }
        const times=[...new Set([...gradient.colors,...gradient.alpha].map(stop=>stop[0]))].sort((a,b)=>a-b);
        const base=tint?.$color3||[0,0,0];
        for(const time of times){
            const [before,after]=gradientSample(gradient.colors,time);
            const [[alphaBefore],[alphaAfter]]=gradientSample(gradient.alpha,time);
            const append=(color,alpha)=>paint.append(svgElement('stop',{offset:time,
                'stop-color':rgb({$color3:color.map((channel,i)=>channel*base[i])}),'stop-opacity':alpha*opacity}));
            append(before,alphaBefore);
            if(alphaBefore!==alphaAfter||before.some((channel,i)=>channel!==after[i]))append(after,alphaAfter);
        }
        const defs=svgElement('defs');
        defs.append(paint);layer.append(defs);
        return 'url(#'+id+')';
    }
    function conicPaint(gradient,origin,stops,color){
        const scale=Math.min(gradient.scale,1);
        const positioned=stops.map(([time,...value])=>color(value)+' '+time*scale+'turn');
        if(gradient.tile===2){
            for(let i=stops.length-2;i>=0;i--){
                const [time,...value]=stops[i];
                positioned.push(color(value)+' '+(2-time)*scale+'turn');
            }
        }
        const name=gradient.tile?'repeating-conic-gradient':'conic-gradient';
        return name+'(from '+(gradient.rotation+90)+'deg at '+origin+', '+positioned.join(', ')+')';
    }
    function appendConicSectors(layer,gradient,width,height,tint,opacity){
        // Local feImage references rasterize SVG shapes but omit foreignObject
        // paint. Use vector sectors for this filter source only.
        const x=width*(.5+gradient.offset[0]),y=height*(.5+gradient.offset[1]);
        const radius=Math.hypot(Math.max(Math.abs(x),Math.abs(width-x)),Math.max(Math.abs(y),Math.abs(height-y)))+1;
        const scale=Math.min(gradient.scale,1),angle=gradient.rotation*Math.PI/180;
        const sectors=256,turn=2*Math.PI;
        const group=svgElement('g',{'shape-rendering':'crispEdges'});
        const base=tint?.$color3||[0,0,0];
        for(let i=0;i<sectors;i++){
            const progress=(i+.5)/(sectors*scale);
            const t=gradient.tile===1?progress%1:gradient.tile===2?1-Math.abs(progress%2-1):Math.min(progress,1);
            const [,color]=gradientSample(gradient.colors,t);
            const [,[alpha]]=gradientSample(gradient.alpha,t);
            if(!alpha)continue;
            const a=angle+turn*i/sectors,b=angle+turn*(i+1)/sectors;
            const start=[x+Math.cos(a)*radius,y+Math.sin(a)*radius];
            const end=[x+Math.cos(b)*radius,y+Math.sin(b)*radius];
            group.append(svgElement('path',{d:`M ${x} ${y} L ${start.join(' ')} L ${end.join(' ')} Z`,
                fill:rgb({$color3:color.map((channel,index)=>channel*base[index])}),'fill-opacity':alpha*opacity}));
        }
        layer.append(group);
    }
    function appendPaintedRect(layer,attributes,channel,{gradient,width,height,color,opacity,padding=0,svgOnly=false}){
        if(gradient?.type!==2){
            attributes[channel]=gradient?gradientPaint(layer,gradient,width,height,color,opacity):rgb(color,opacity);
            layer.append(svgElement('rect',attributes));
            return;
        }
        if(svgOnly){appendConicSectors(layer,gradient,width,height,color,opacity);return;}
        const bounds={x:-padding,y:-padding,width:width+2*padding,height:height+2*padding};
        const id='rove-gradient-'+(++nextGradient);
        const mask=svgElement('mask',{...bounds,id,maskUnits:'userSpaceOnUse',maskContentUnits:'userSpaceOnUse'});
        mask.append(svgElement('rect',{...attributes,[channel]:'white'}));
        const defs=svgElement('defs');
        defs.append(mask);
        const foreign=svgElement('foreignObject',{...bounds,mask:'url(#'+id+')'});
        const surface=document.createElement('div');
        const x=width*(.5+gradient.offset[0])+padding,y=height*(.5+gradient.offset[1])+padding;
        const origin=x+'px '+y+'px';
        const base=color?.$color3||[0,0,0];
        Object.assign(surface.style,{width:'100%',height:'100%',opacity:String(opacity),
            backgroundImage:conicPaint(gradient,origin,gradient.colors,value=>rgb({$color3:value.map((component,i)=>component*base[i])})),
            maskImage:conicPaint(gradient,origin,gradient.alpha,([alpha])=>'rgba(0,0,0,'+alpha+')')});
        foreign.append(surface);
        layer.append(defs,foreign);
    }
    function gradientFilter(layer,gradient,width,height,color){
        const id='rove-gradient-'+(++nextGradient);
        const region={x:0,y:0,width,height};
        const paint=svgElement('g',{id:id+'-source'});
        appendPaintedRect(paint,{width,height},'fill',{gradient,width,height,color,opacity:1,svgOnly:true});
        const filter=svgElement('filter',{...region,id,filterUnits:'userSpaceOnUse',
            primitiveUnits:'userSpaceOnUse','color-interpolation-filters':'sRGB'});
        filter.append(svgElement('feImage',{...region,href:'#'+id+'-source',result:'paint'}));
        // Multiplication of premultiplied inputs preserves both alpha channels.
        filter.append(svgElement('feComposite',{in:'SourceGraphic',in2:'paint',operator:'arithmetic',k1:1}));
        const defs=svgElement('defs');
        defs.append(paint,filter);layer.append(defs);
        return 'url(#'+id+')';
    }
    function updateTextGradient(entry,g){
        const e=entry.element,gradient=g.backgroundGradient;
        if(!gradient||g.class==='TextBox'){
            if(entry.textGradient){
                entry.textGradient.remove();
                entry.textGradient=null;
                e.style.filter='';
            }
            return;
        }
        if(!entry.textGradient){
            entry.textGradient=svgElement('svg',{class:'gui-text-gradient','aria-hidden':true});
            e.before(entry.textGradient);
            entry.textGradientSignature=null;
        }
        const [, ,width,height]=g.box;
        const signature=JSON.stringify([width,height,gradient]);
        if(entry.textGradientSignature===signature)return;
        entry.textGradientSignature=signature;
        entry.textGradient.replaceChildren();
        e.style.filter=gradientFilter(entry.textGradient,gradient,width,height,{$color3:[1,1,1]});
    }
    function updateGradientFill(entry,g,opacity){
        if(!g.backgroundGradient||!opacity){
            entry.gradientFill?.remove();
            entry.gradientFill=null;
            return false;
        }
        if(!entry.gradientFill){
            entry.gradientFill=svgElement('svg',{class:'game-node gui-gradient-fill','aria-hidden':true});
            entry.element.before(entry.gradientFill);
            entry.gradientSignature=null;
        }
        const layer=entry.gradientFill;
        syncGuiLayer(layer,entry.element);
        const [, ,width,height]=g.box;
        const signature=JSON.stringify([width,height,g.cornerRadius,g.background,g.backgroundGradient,opacity]);
        if(entry.gradientSignature!==signature){
            entry.gradientSignature=signature;
            layer.replaceChildren();
            appendPaintedRect(layer,{width,height,rx:Math.max(0,Math.min(width/2,height/2,finite(g.cornerRadius)))},'fill',
                {gradient:g.backgroundGradient,width,height,color:g.background,opacity});
        }
        return true;
    }
    function updateBorderStrokes(entry,g){
        const strokes=g.borderStrokes??(g.stroke?[g.stroke]:[]);
        if(!strokes.length){
            entry.borders?.remove();
            entry.borders=null;
            return;
        }
        const e=entry.element;
        if(!entry.borders){
            entry.borders=svgElement('svg',{class:'game-node gui-border-strokes','aria-hidden':true});
            e.after(entry.borders);
            entry.borderSignature=null;
        }
        const layer=entry.borders;
        syncGuiLayer(layer,e);
        const [, ,width,height]=g.box;
        const radius=Math.max(0,Math.min(width/2,height/2,finite(g.cornerRadius)));
        const signature=JSON.stringify([width,height,radius,strokes]);
        if(entry.borderSignature===signature)return;
        entry.borderSignature=signature;
        layer.replaceChildren();
        for(const stroke of strokes){
            const thickness=Math.max(0,Math.min(1000,finite(stroke.thickness)));
            if(!thickness)continue;
            const position=[0,1,2].includes(stroke.position)?stroke.position:1;
            // Offset moves the center line. If it collapses, the remaining
            // outer extent becomes a filled shape.
            const shift=finite(stroke.offset)+(1-position)*thickness/2;
            const filled=width+2*shift<=0||height+2*shift<=0;
            const edge=filled?shift+thickness/2:shift;
            const w=width+2*edge,h=height+2*edge;
            if(w<=0||h<=0)continue;
            const opacity=Math.max(0,Math.min(1,1-finite(stroke.transparency)));
            const attrs={x:-edge,y:-edge,width:w,height:h,
                rx:radius?Math.max(0,Math.min(w/2,h/2,radius+edge)):0,
                fill:'none',stroke:'none','stroke-width':thickness,
                'stroke-linejoin':['round','bevel','miter'][stroke.join]||'round'};
            const padding=Math.max(0,edge+(filled?0:thickness/2));
            appendPaintedRect(layer,attrs,filled?'fill':'stroke',{gradient:stroke.gradient,width,height,color:stroke.color,opacity,padding});
        }
    }
    function removeGuiEntry(entry){entry.image=null;entry.gradientFill?.remove();entry.textGradient?.remove();entry.borders?.remove();RoveText.remove(entry);}
    function updateGui(frame) {
        fonts.observe(frame.gui);
        const seen=new Set();
        for(const g of frame.gui||[]) {
            if(!Array.isArray(g.box)||g.box.length!==5||g.box.some(x=>!Number.isFinite(x)))continue;
            seen.add(g.id);let entry=nodes.get(g.id);
            if(!entry) {
                const element=document.createElement(g.class==='TextBox'?'textarea':g.interactive?'button':'div');element.className='game-node'+(g.interactive?' interactive':'');element.dataset.instance=g.id;element.setAttribute('aria-label',String(g.name));
                element.addEventListener('click',async()=>{
                    const id=element.dataset.pointerTarget;
                    if(id&&await guiAction({id,kind:'pointer_down'}))await guiAction({id,kind:'pointer_up'});
                });
                // GUI controls own their input; world movement never steals focus.
                for(const event of ['pointerdown','keydown','keyup'])element.addEventListener(event,e=>{if(g.interactive)e.stopPropagation();});
                $('game-gui').append(element);entry={element,image:null};nodes.set(g.id,entry);
                if(g.class==='TextBox')bindTextBox(entry);
            }
            const e=entry.element,[x,y,w,h,angle]=g.box;
            if(g.class==='ImageButton')e.disabled=g.imageButtonEnabled===false;
            e.dataset.pointerTarget=g.pointerTarget||'';e.classList.toggle('interactive',!!g.interactive);
            e.setAttribute('aria-label',String(g.contentText||g.text||g.name));
            e.style.left=x+'px';e.style.top=y+'px';e.style.width=w+'px';e.style.height=h+'px';e.style.transform='rotate('+angle+'deg)';
            RoveGuiClip.apply(e,g);
            scrolling.update(e,g);
            // Background alpha must not fade text or children.
            e.style.borderRadius=Math.max(0,Math.min(w/2,h/2,finite(g.cornerRadius)))+'px';
            e.style.zIndex=String(Math.max(-100000,Math.min(100000,finite(g.displayOrder)*1000+finite(g.z,1))));
            const backgroundAlpha=Math.max(0,Math.min(1,1-finite(g.backgroundTransparency)));
            e.style.backgroundColor=updateGradientFill(entry,g,backgroundAlpha)?'transparent':rgb(g.background,backgroundAlpha);
            updateBorderStrokes(entry,g);
            const textAlpha=Math.max(0,Math.min(1,1-finite(g.textTransparency)));
            e.style.color=rgb(g.textColor,textAlpha);
            if(g.class==='TextBox')e.style.setProperty('--placeholder-color',rgb(g.placeholderColor,textAlpha));
            entry.font=g.fontFace;fonts.apply(e,g.fontFace);
            entry.textFrame=RoveText.isText(g)?g:null;
            if(entry.textFrame){
                RoveText.apply(entry,g,fonts,fontGeneration);
                updateTextGradient(entry,g);
            }
            else if(e.tagName==='INPUT'){e.readOnly=g.textEditable===false;if(document.activeElement!==e)e.value=String(g.text||'');}
            else if(g.image||g.imageHover||g.imagePressed||entry.image)RoveImage.apply(entry,g,gradientFilter,reportImageLoaded);
            else if(e.textContent!==String(g.text||''))e.textContent=String(g.text||'');
        }
        for(const [id,entry] of nodes)if(!seen.has(id)){removeGuiEntry(entry);nodes.delete(id);}
        applyTextFocus(frame.textFocus,frame.textEditor);
    }
    function updatePlayers(players) {
        const serialized=JSON.stringify(players);if(lastPlayers===serialized)return;lastPlayers=serialized;$('player-list').replaceChildren();
        for(const player of players){const row=document.createElement('div');row.textContent=player.displayName||player.name;if(player.local){const own=document.createElement('small');own.textContent='You';row.append(own);}$('player-list').append(row);}
    }
    function clearFrame() {
        scrolling.reset();
        fonts.clear();
        RoveText.clear();
        textFocus.mirroring=true;
        for(const entry of nodes.values())removeGuiEntry(entry);nodes.clear();
        textFocus=freshTextFocus();
        renderer.frame({parts:[],gui:[],players:[]});hasFrame=false;lastSample=null;
        $('player-list').replaceChildren();lastPlayers='';
    }
    function presentFrame(frame,sample) {
        renderer.frame(frame);updateGui(frame);updatePlayers(frame.players||[]);
        hasFrame=true;lastSample=sample;
        $('statistics').textContent=(renderer.backend==='software'?'Software · ':'GPU · ')+renderer.fps+' fps · '+frame.parts.length+' parts · '+frame.gui.length+' UI';
        const a=renderer.stats;$('render-details').textContent=(renderer.backend==='software'?'Software rendering at reduced resolution; GPU shadows are unavailable. ':'')+(renderer.reducedDetail?'Geometry budget reached; some geometry is omitted. ':'')+a.ready+' loaded assets · '+a.pending+' loading · '+a.failed+' unavailable · '+a.meshBounds+' mesh bounds · '+frame.skippedParts+' omitted parts. '+(a.skinnedMeshes?a.skinnedMeshes+' meshes use their bind pose; bone deformation is not supported. ':'')+(frame.terrainUnsupported?'Terrain voxels are not rendered. ':'')+'R15 clothing composition, layered clothing deformation, particles, and some GUI layouts remain unsupported.';
    }
    async function tick() {
        if(stopped)return;
        const started=performance.now();
        let frameInterval=100;
        try{
            if(!document.hidden&&!document.getElementById('workspace-page').hidden){
                if(!connected||Date.now()-lastTick>1500){const response=await fetch('/api/runtime/status',{signal:AbortSignal.timeout(5000)});if(!response.ok)throw new Error('The local client is temporarily unavailable.');const state=await response.json();
                    if(state.session!==runtimeIdentity){clearFrame();runtimeIdentity=state.session;lastSize='';resizeRetry=0;held.clear();moving=moveQueued=releaseQueued=false;controls.setConnected(false);renderer.setMode($('camera').value);}
                    connected=state.connected===true;if(!connected)lastSize='';$('leave').disabled=!connected;controls.setConnected(connected);lastTick=Date.now();}
                if(!connected)throw new Error('Start a game from the launcher or terminal. This window attaches without rejoining.');
                void syncSize();
                const result=await post('/api/runtime/render',lastSample==null?{}:{after:lastSample});if(stopped)return;
                if(moving||moveBusy||(!hasFrame&&result.pending))frameInterval=50;
                if(result.frame&&(result.sample==null||result.sample!==lastSample))presentFrame(result.frame,result.sample);
                const delayed=!!result.message||(result.age_ms||0)>2000;
                $('waiting').hidden=hasFrame;
                $('waiting-text').textContent=result.message||'Preparing your game view. The client is still responding…';
                $('connection').textContent=!hasFrame?(result.message?'Game frame unavailable':'Preparing game view'):delayed?'Connected · waiting for frame':'● Live · same session';
                $('connection').classList.toggle('live',hasFrame&&!delayed);
                if(hasFrame&&delayed)$('statistics').textContent='Showing the last frame · waiting for the client';
            }
        }catch(error){
            $('connection').textContent=connected?'Waiting for game frame':'Waiting for session';$('connection').classList.remove('live');
            $('waiting').hidden=connected&&hasFrame;$('waiting-text').textContent=requestMessage(error);
            if(!connected){clearFrame();$('statistics').textContent='Waiting for a connected session';}
            else if(hasFrame)$('statistics').textContent='Showing the last frame · reconnecting to the display';
        }finally{if(!stopped)setTimeout(tick,Math.max(25,frameInterval-(performance.now()-started)));}
    }
    $('camera').addEventListener('change',()=>renderer.setMode($('camera').value));
    $('players-toggle').onclick=()=>{$('players').hidden=!$('players').hidden;};
    $('details-toggle').onclick=()=>{$('details').hidden=!$('details').hidden;};
    $('details-close').onclick=()=>{$('details').hidden=true;};
    $('fullscreen').onclick=()=>{if(document.fullscreenElement)void document.exitFullscreen();else void $('surface').requestFullscreen?.();};
    $('leave').onclick=()=>{$('leave-dialog').showModal();resetMovement();};$('cancel-leave').onclick=()=>$('leave-dialog').close();
    $('confirm-leave').onclick=async()=>{const button=$('confirm-leave');button.disabled=true;try{await post('/api/runtime/leave',{});connected=false;controls.setConnected(false);$('leave-dialog').close();$('waiting').hidden=false;$('waiting-text').textContent='Session ended. You can join another game from the launcher.';}catch(error){$('action-state').textContent=error.message;$('leave-dialog').close();$('details').hidden=false;}finally{button.disabled=false;}};
    createChatControls({input:$('chat-text'),button:$('chat-form').querySelector('button'),form:$('chat-form'),channel:$('chat-channel'),
        available:()=>connected,submit:data=>post('/api/runtime/user-chat',data),
        onResult:()=>$('chat-state').textContent='Submitted · waiting for server delivery',
        onError:message=>$('chat-state').textContent=message});
    window.addEventListener('pagehide',()=>{resetMovement();stopped=true;scrolling.destroy();controls.destroy();renderer.dispose();fonts.dispose();RoveText.dispose();});
    $('scripts-toggle').onclick=()=>{const panel=document.getElementById('script-workspace');panel.hidden=!panel.hidden;$('scripts-toggle').setAttribute('aria-expanded',String(!panel.hidden));window.dispatchEvent(new Event('resize'));};
    window.addEventListener('rove:navigate',()=>{resetMovement();scrolling.reset();controls.setConnected(false);lastSize='';});
    void tick();
});
