//! Read-only Enum and EnumItem objects, backed by the pinned native enum catalog.
use mlua::{
    AnyUserData, Lua, MetaMethod, Result, UserData, UserDataFields, UserDataMethods, Value,
};
use serde_json::{json, Value as Json};
use std::collections::BTreeMap;
use std::sync::OnceLock;

#[cfg(test)]
#[path = "enums_tests.rs"]
mod tests;

#[derive(Clone, Debug)]
struct Item {
    name: String,
    value: i32,
}
#[derive(Clone, Debug)]
struct EnumType {
    name: String,
    items: mlua::Table,
}
#[derive(Clone, Debug)]
struct EnumItem {
    enum_name: String,
    name: String,
    value: i32,
}

// Generated from runtime/*_enum_data.inc and runtime/*_enum_specs.inc and
// runtime/export_enums.inc. Keeping this pinned copy means runtime startup does
// not parse C++ source or depend on the old native host.
const DATA: &str = r#"AccessoryType|Unknown=0,Hat=1,Hair=2,Face=3,Neck=4,Shoulder=5,Front=6,Back=7,Waist=8,TShirt=9,Shirt=10,Pants=11,Jacket=12,Sweater=13,Shorts=14,LeftShoe=15,RightShoe=16,DressSkirt=17,Eyebrow=18,Eyelash=19
CameraMode|Classic=0,LockFirstPerson=1
CameraType|Fixed=0,Attach=1,Watch=2,Track=3,Follow=4,Custom=5,Scriptable=6,Orbital=7
ComputerCameraMovementMode|Default=0,Classic=1,Follow=2,Orbital=3,CameraToggle=4
ComputerMovementMode|Default=0,KeyboardMouse=1,ClickToMove=2
ControlMode|Classic=0,MouseLockSwitch=1
CustomCameraMode|Default=0,Classic=1,Follow=2
DevCameraOcclusionMode|Zoom=0,Invisicam=1
DevComputerCameraMovementMode|UserChoice=0,Classic=1,Follow=2,Orbital=3,CameraToggle=4
DevComputerMovementMode|UserChoice=0,KeyboardMouse=1,ClickToMove=2,Scriptable=3
DevTouchCameraMovementMode|UserChoice=0,Classic=1,Follow=2,Orbital=3
DevTouchMovementMode|UserChoice=0,Thumbstick=1,DPad=2,Thumbpad=3,ClickToMove=4,Scriptable=5,DynamicThumbstick=6
FormFactor|Symmetric=0,Brick=1,Plate=2,Custom=3
HumanoidRigType|R6=0,R15=1
OrientationAlignmentMode|OneAttachment=0,TwoAttachment=1
PathStatus|Success=0,ClosestNoPath=1,ClosestOutOfRange=2,FailStartNotEmpty=3,FailFinishNotEmpty=4,NoPath=5
PathWaypointAction|Walk=0,Jump=1,Custom=2
PreferredInput|KeyboardAndMouse=0,Gamepad=1,Touch=2,MicroGamepad=3
RolloutState|Default=0,Disabled=1,Enabled=2
RotationType|MovementRelative=0,CameraRelative=1
ScreenOrientation|LandscapeLeft=0,LandscapeRight=1,LandscapeSensor=2,Portrait=3,Sensor=4
TouchCameraMovementMode|Default=0,Classic=1,Follow=2,Orbital=3
TouchMovementMode|Default=0,Thumbstick=1,DPad=2,Thumbpad=3,ClickToMove=4,DynamicThumbstick=5
UserCFrame|Head=0,LeftHand=1,RightHand=2,Floor=3
VRTouchpad|Left=0,Right=1
VRTouchpadMode|Touch=0,VirtualThumbstick=1,ABXY=2
ZIndexBehavior|Global=0,Sibling=1
CatalogSortType|Relevance=1,PriceHighToLow=2,PriceLowToHigh=3,MostFavorited=5,RecentlyCreated=6,Bestselling=7
CatalogSortAggregation|Past12Hours=1,PastDay=2,Past3Days=3,PastWeek=4,PastMonth=5,AllTime=6
CatalogCategoryFilter|None=1,Featured=2,Collectibles=3,CommunityCreations=4,Premium=5,Recommended=6
SalesTypeFilter|All=1,Collectibles=2,Premium=3,TimedOptions=4
BundleType|BodyParts=1,Animations=2,Shoes=3,DynamicHead=4,DynamicHeadAvatar=5
AvatarAssetType|TShirt=2,Hat=8,Shirt=11,Pants=12,Head=17,Face=18,Gear=19,Torso=27,RightArm=28,LeftArm=29,LeftLeg=30,RightLeg=31,HairAccessory=41,FaceAccessory=42,NeckAccessory=43,ShoulderAccessory=44,FrontAccessory=45,BackAccessory=46,WaistAccessory=47,ClimbAnimation=48,FallAnimation=50,IdleAnimation=51,JumpAnimation=52,RunAnimation=53,SwimAnimation=54,WalkAnimation=55,EmoteAnimation=61,TShirtAccessory=64,ShirtAccessory=65,PantsAccessory=66,JacketAccessory=67,SweaterAccessory=68,ShortsAccessory=69,LeftShoeAccessory=70,RightShoeAccessory=71,DressSkirtAccessory=72,EyebrowAccessory=76,EyelashAccessory=77,MoodAnimation=78,DynamicHead=79,FaceMakeup=88,LipMakeup=89,EyeMakeup=90,AvatarBackground=92
CreatorTypeFilter|User=0,Group=1,All=2
KeyInterpolationMode|Constant=0,Linear=1,Cubic=2
ContentSourceType|None=0,Uri=1,Object=2,Opaque=3
SecurityCapability|RunClientScript=0,RunServerScript=1,AccessOutsideWrite=2,AssetRequire=3,LoadString=4,ScriptGlobals=5,CreateInstances=6,Basic=7,Audio=8,DataStore=9,Network=10,Physics=11,UI=12,CSG=13,Chat=14,Animation=15,Avatar=16,Input=17,Environment=18,RemoteEvent=19,LegacySound=20,Players=21,CapabilityControl=22,Plugin=23,LocalUser=24,WritePlayer=25,RobloxScript=26,RobloxEngine=27,Unassigned=28,InternalTest=29,PluginOrOpenCloud=30,Assistant=31,RemoteCommand=32,AssetRead=33,AssetManagement=34,DynamicGeneration=35,PlatformAvatarEditing=36,AssetCreateUpdate=37,Capture=38,SensitiveInput=39,Monetization=40,LoadOwnedAsset=41,Social=42,ServerCommunication=43,Logging=44,PromptExternalPurchase=45,Groups=46,Teleport=47,Consequences=48,Material=49,AvatarBehavior=50,AvatarAppearance=51,LoadUnownedAsset=52
Material|Plastic=256,SmoothPlastic=272,Neon=288,Wood=512,WoodPlanks=528,Marble=784,Basalt=788,Slate=800,CrackedLava=804,Concrete=816,Limestone=820,Granite=832,Pavement=836,Brick=848,Pebble=864,Cobblestone=880,Rock=896,Sandstone=912,CorrodedMetal=1040,DiamondPlate=1056,Foil=1072,Metal=1088,Grass=1280,LeafyGrass=1284,Sand=1296,Fabric=1312,Snow=1328,Mud=1344,Ground=1360,Asphalt=1376,Salt=1392,Ice=1536,Glacier=1552,Glass=1568,ForceField=1584,Air=1792,Water=2048,Cardboard=2304,Carpet=2305,CeramicTiles=2306,ClayRoofTiles=2307,RoofShingles=2308,Leather=2309,Plaster=2310,Rubber=2311
CollisionFidelity|Default=0,Hull=1,Box=2,PreciseConvexDecomposition=3,Tunable=4
MouseBehavior|Default=0,LockCenter=1,LockCurrentPosition=2
UserInputState|Begin=0,Change=1,End=2,Cancel=3,None=4
UserInputType|MouseButton1=0,MouseButton2=1,MouseButton3=2,MouseWheel=3,MouseMovement=4,Touch=7,Keyboard=8,Focus=9,Accelerometer=10,Gyro=11,Gamepad1=12,Gamepad2=13,Gamepad3=14,Gamepad4=15,Gamepad5=16,Gamepad6=17,Gamepad7=18,Gamepad8=19,TextInput=20,InputMethod=21,None=22
KeyCode|None=0,Backspace=8,Tab=9,Clear=12,Return=13,Pause=19,Escape=27,Space=32,QuotedDouble=34,Hash=35,Dollar=36,Percent=37,Ampersand=38,Quote=39,LeftParenthesis=40,RightParenthesis=41,Asterisk=42,Plus=43,Comma=44,Minus=45,Period=46,Slash=47,Zero=48,One=49,Two=50,Three=51,Four=52,Five=53,Six=54,Seven=55,Eight=56,Nine=57,Colon=58,Semicolon=59,LessThan=60,Equals=61,GreaterThan=62,Question=63,At=64,LeftBracket=91,BackSlash=92,RightBracket=93,Caret=94,Underscore=95,Backquote=96,A=97,B=98,C=99,D=100,E=101,F=102,G=103,H=104,I=105,J=106,K=107,L=108,M=109,N=110,O=111,P=112,Q=113,R=114,S=115,T=116,U=117,V=118,W=119,X=120,Y=121,Z=122,LeftCurly=123,Pipe=124,RightCurly=125,Tilde=126,Delete=127,World0=160,World1=161,World2=162,World3=163,World4=164,World5=165,World6=166,World7=167,World8=168,World9=169,World10=170,World11=171,World12=172,World13=173,World14=174,World15=175,World16=176,World17=177,World18=178,World19=179,World20=180,World21=181,World22=182,World23=183,World24=184,World25=185,World26=186,World27=187,World28=188,World29=189,World30=190,World31=191,World32=192,World33=193,World34=194,World35=195,World36=196,World37=197,World38=198,World39=199,World40=200,World41=201,World42=202,World43=203,World44=204,World45=205,World46=206,World47=207,World48=208,World49=209,World50=210,World51=211,World52=212,World53=213,World54=214,World55=215,World56=216,World57=217,World58=218,World59=219,World60=220,World61=221,World62=222,World63=223,World64=224,World65=225,World66=226,World67=227,World68=228,World69=229,World70=230,World71=231,World72=232,World73=233,World74=234,World75=235,World76=236,World77=237,World78=238,World79=239,World80=240,World81=241,World82=242,World83=243,World84=244,World85=245,World86=246,World87=247,World88=248,World89=249,World90=250,World91=251,World92=252,World93=253,World94=254,World95=255,KeypadZero=256,KeypadOne=257,KeypadTwo=258,KeypadThree=259,KeypadFour=260,KeypadFive=261,KeypadSix=262,KeypadSeven=263,KeypadEight=264,KeypadNine=265,KeypadPeriod=266,KeypadDivide=267,KeypadMultiply=268,KeypadMinus=269,KeypadPlus=270,KeypadEnter=271,KeypadEquals=272,Up=273,Down=274,Right=275,Left=276,Insert=277,Home=278,End=279,PageUp=280,PageDown=281,F1=282,F2=283,F3=284,F4=285,F5=286,F6=287,F7=288,F8=289,F9=290,F10=291,F11=292,F12=293,F13=294,F14=295,F15=296,NumLock=300,CapsLock=301,ScrollLock=302,RightShift=303,LeftShift=304,RightControl=305,LeftControl=306,RightAlt=307,LeftAlt=308,RightMeta=309,LeftMeta=310,LeftSuper=311,RightSuper=312,Mode=313,Compose=314,Help=315,Print=316,SysReq=317,Break=318,Menu=319,Power=320,Euro=321,Undo=322,ButtonX=1000,ButtonY=1001,ButtonA=1002,ButtonB=1003,ButtonR1=1004,ButtonL1=1005,ButtonR2=1006,ButtonL2=1007,ButtonR3=1008,ButtonL3=1009,ButtonStart=1010,ButtonSelect=1011,DPadLeft=1012,DPadRight=1013,DPadUp=1014,DPadDown=1015,Thumbstick1=1016,Thumbstick2=1017,Thumbstick1Up=1018,Thumbstick1Down=1019,Thumbstick1Left=1020,Thumbstick1Right=1021,Thumbstick2Up=1022,Thumbstick2Down=1023,Thumbstick2Left=1024,Thumbstick2Right=1025,MouseLeftButton=1026,MouseRightButton=1027,MouseMiddleButton=1028,MouseBackButton=1029,MouseNoButton=1030,MouseX=1031,MouseY=1032,MousePosition=1033,TouchPosition=1034,MouseWheel=1035,TrackpadPan=1040,TrackpadPinch=1045,MouseDelta=1048,TouchDelta=1049,TouchPinch=1050,ButtonCenter=1051,ButtonBack=1052,ButtonUp=1053,ButtonDown=1054,ButtonLeft=1055,ButtonRight=1056
ContextActionPriority|Low=1000,Medium=2000,High=3000
ContextActionResult|Sink=0,Pass=1
PlayerActions|CharacterForward=0,CharacterBackward=1,CharacterLeft=2,CharacterRight=3,CharacterJump=4
BulkMoveMode|FireAllEvents=0,FireCFrameChanged=1
MeshType|Head=0,Torso=1,Wedge=2,Sphere=3,Cylinder=4,FileMesh=5,Brick=6,Prism=7,Pyramid=8,ParallelRamp=9,RightAngleRamp=10,CornerWedge=11
RunContext|Legacy=0,Server=1,Client=2,Plugin=3
Axis|X=0,Y=1,Z=2
NormalId|Right=0,Top=1,Back=2,Left=3,Bottom=4,Front=5
RotationOrder|XYZ=0,XZY=1,YZX=2,YXZ=3,ZXY=4,ZYX=5
SizeConstraint|RelativeXY=0,RelativeXX=1,RelativeYY=2
AutomaticSize|None=0,X=1,Y=2,XY=3
AspectType|FitWithinMaxSize=0,ScaleWithParentSize=1
DominantAxis|Width=0,Height=1
Font|Legacy=0,Arial=1,ArialBold=2,SourceSans=3,SourceSansBold=4,SourceSansLight=5,SourceSansItalic=6,Bodoni=7,Garamond=8,Cartoon=9,Code=10,Highway=11,SciFi=12,Arcade=13,Fantasy=14,Antique=15,SourceSansSemibold=16,Gotham=17,GothamMedium=18,GothamBold=19,GothamBlack=20,AmaticSC=21,Bangers=22,Creepster=23,DenkOne=24,Fondamento=25,FredokaOne=26,GrenzeGotisch=27,IndieFlower=28,JosefinSans=29,Jura=30,Kalam=31,LuckiestGuy=32,Merriweather=33,Michroma=34,Nunito=35,Oswald=36,PatrickHand=37,PermanentMarker=38,Roboto=39,RobotoCondensed=40,RobotoMono=41,Sarpanch=42,SpecialElite=43,TitilliumWeb=44,Ubuntu=45,BuilderSans=46,BuilderSansMedium=47,BuilderSansBold=48,BuilderSansExtraBold=49,Arimo=50,ArimoBold=51,Unknown=100
MessageType|MessageOutput=0,MessageInfo=1,MessageWarning=2,MessageError=3
EasingStyle|Linear=0,Sine=1,Back=2,Quad=3,Quart=4,Quint=5,Bounce=6,Elastic=7,Exponential=8,Circular=9,Cubic=10
EasingDirection|In=0,Out=1,InOut=2
PlaybackState|Begin=0,Delayed=1,Playing=2,Paused=3,Completed=4,Cancelled=5
RenderPriority|First=0,Input=100,Camera=200,Character=300,Last=2000
PartType|Ball=0,Block=1,Cylinder=2,Wedge=3,CornerWedge=4
ActuatorRelativeTo|Attachment0=0,Attachment1=1,World=2
VelocityConstraintMode|Line=0,Plane=1,Vector=2
ForceLimitMode|Magnitude=0,PerAxis=1
FontWeight|Thin=100,ExtraLight=200,Light=300,Regular=400,Medium=500,SemiBold=600,Bold=700,ExtraBold=800,Heavy=900
FontStyle|Normal=0,Italic=1
FillDirection|Horizontal=0,Vertical=1
HorizontalAlignment|Center=0,Left=1,Right=2
VerticalAlignment|Center=0,Top=1,Bottom=2
SortOrder|Name=0,Custom=1,LayoutOrder=2
UIFlexAlignment|None=0,Fill=1,SpaceAround=2,SpaceBetween=3,SpaceEvenly=4
ItemLineAlignment|Automatic=0,Start=1,Center=2,End=3,Stretch=4
TextXAlignment|Left=0,Right=1,Center=2
TextYAlignment|Top=0,Center=1,Bottom=2
TextDirection|Auto=0,LeftToRight=1,RightToLeft=2
ScrollingDirection|X=1,Y=2,XY=4
ScrollBarInset|None=0,ScrollBar=1,Always=2
VerticalScrollBarPosition|Right=0,Left=1
UIFlexMode|None=0,Grow=1,Shrink=2,Fill=3,Custom=4
StartCorner|TopLeft=0,TopRight=1,BottomLeft=2,BottomRight=3
RaycastFilterType|Exclude=0,Include=1
TextTruncate|None=0,AtEnd=1,SplitWord=2
ApplyStrokeMode|Contextual=0,Border=1
StrokeSizingMode|FixedSize=0,ScaledSize=1
BorderStrokePosition|Outer=0,Center=1,Inner=2
LineJoinMode|Round=0,Bevel=1,Miter=2
GradientTileMode|Clamp=0,Repeat=1,Mirror=2
GradientType|Linear=0,Radial=1,Conical=2,Elliptical=3
ScaleType|Stretch=0,Slice=1,Tile=2,Fit=3,Crop=4
ResamplerMode|Default=0,Pixelated=1
CompressionAlgorithm|Zstd=0
HashAlgorithm|Blake2b=0,Blake3=1,Md5=2,Sha1=3,Sha256=4
AnimationPriority|Idle=0,Movement=1,Action=2,Action2=3,Action3=4,Action4=5,Core=1000
TextChatMessageStatus|Unknown=1,Success=2,Sending=3,TextFilterFailed=4,Floodchecked=5,InvalidPrivacySettings=6,InvalidTextChannelPermissions=7,MessageTooLong=8,ModerationTimeout=9
InitialDockState|Top=0,Bottom=1,Left=2,Right=3,Float=4
"#;

fn catalog() -> &'static BTreeMap<String, Vec<Item>> {
    static CATALOG: OnceLock<BTreeMap<String, Vec<Item>>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        DATA.lines()
            .filter_map(|line| {
                let (en, rest) = line.split_once('|')?;
                let items = rest
                    .split(',')
                    .filter_map(|part| {
                        let (name, val) = part.split_once('=')?;
                        Some(Item {
                            name: name.to_owned(),
                            value: val.parse().ok()?,
                        })
                    })
                    .collect::<Vec<_>>();
                Some((en.to_owned(), items))
            })
            .collect()
    })
}

impl UserData for EnumType {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_meta_field("__type", "Enum");
        fields.add_field_method_get("Name", |_, this| Ok(this.name.clone()));
    }
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| Ok(this.name.clone()));
        methods.add_meta_method(
            MetaMethod::Index,
            |lua, this, key: String| -> Result<Value> {
                if key == "GetEnumItems" {
                    let name = this.name.clone();
                    return Ok(Value::Function(
                        lua.create_function(move |lua, _: Value| enum_items(lua, &name))?,
                    ));
                }
                if key == "FromName" {
                    let name = this.name.clone();
                    return Ok(Value::Function(lua.create_function(
                        move |lua, value: String| {
                            let lookup = enum_alias(&name, &value);
                            Ok(
                                match enum_type_item_table(lua, &name)
                                    .ok()
                                    .and_then(|t| t.get::<Value>(lookup).ok())
                                {
                                    Some(v) => v,
                                    None => Value::Nil,
                                },
                            )
                        },
                    )?));
                }
                if key == "FromValue" {
                    let name = this.name.clone();
                    return Ok(Value::Function(lua.create_function(
                        move |lua, value: f64| {
                            let item = catalog()
                                .get(&name)
                                .and_then(|items| items.iter().find(|it| it.value as f64 == value));
                            let item_name = match item {
                                Some(it) => &it.name,
                                None => return Ok(Value::Nil),
                            };
                            Ok(enum_type_item_table(lua, &name)?
                                .get::<Value>(item_name.as_str())?)
                        },
                    )?));
                }
                let lookup = enum_alias(&this.name, &key);
                let value = this.items.get::<Value>(lookup)?;
                if matches!(value, Value::Nil) {
                    Err(mlua::Error::RuntimeError(format!(
                        "unknown Enum.{} member '{key}'",
                        this.name
                    )))
                } else {
                    Ok(value)
                }
            },
        );
        methods.add_meta_method(
            MetaMethod::NewIndex,
            |_, this, (key, _): (Value, Value)| -> Result<()> {
                Err(mlua::Error::RuntimeError(format!(
                    "Enum.{} is read-only ({key:?})",
                    this.name
                )))
            },
        );
    }
}

fn enum_alias<'a>(enum_name: &str, item_name: &'a str) -> &'a str {
    match (enum_name, item_name) {
        ("KeyCode", "Unknown") => "None",
        ("RaycastFilterType", "Blacklist") => "Exclude",
        ("RaycastFilterType", "Whitelist") => "Include",
        _ => item_name,
    }
}

impl UserData for EnumItem {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_meta_field("__type", "EnumItem");
        fields.add_field_method_get("Name", |_, this| Ok(this.name.clone()));
        fields.add_field_method_get("Value", |_, this| Ok(this.value));
        fields.add_field_method_get("EnumType", |lua, this| enum_type(lua, &this.enum_name));
    }
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!("Enum.{}.{}", this.enum_name, this.name))
        });
        methods.add_meta_method(MetaMethod::Eq, |_, this, other: AnyUserData| {
            Ok(other
                .borrow::<EnumItem>()
                .map(|b| this.enum_name == b.enum_name && this.name == b.name)
                .unwrap_or(false))
        });
        methods.add_meta_method(
            MetaMethod::NewIndex,
            |_, _, (key, _): (Value, Value)| -> Result<()> {
                Err(mlua::Error::RuntimeError(format!(
                    "EnumItem is read-only ({key:?})"
                )))
            },
        );
    }
}

fn enum_type_item_table(lua: &Lua, name: &str) -> Result<mlua::Table> {
    // The public Enum table is immutable and stores the canonical Enum userdata.
    // Keeping this lookup central preserves EnumType identity on EnumItem access.
    let root: mlua::Table = lua.globals().get("Enum")?;
    let en: AnyUserData = root.get(name)?;
    Ok(en.borrow::<EnumType>()?.items.clone())
}
fn enum_type(lua: &Lua, name: &str) -> Result<AnyUserData> {
    let enums: mlua::Table = lua.globals().get("Enum")?;
    enums.get(name)
}
fn enum_items(lua: &Lua, name: &str) -> Result<mlua::Table> {
    let definitions = catalog();
    let definitions = definitions
        .get(name)
        .ok_or_else(|| mlua::Error::RuntimeError(format!("unknown enum {name}")))?;
    let type_obj = enum_type(lua, name)?;
    let items = type_obj.borrow::<EnumType>()?.items.clone();
    let table = lua.create_table_with_capacity(definitions.len(), 0)?;
    for (index, item) in definitions.iter().enumerate() {
        table.raw_set(index + 1, items.get::<Value>(item.name.as_str())?)?;
    }
    Ok(table)
}
pub fn enum_item(lua: &Lua, enum_name: &str, item_name: &str) -> Result<AnyUserData> {
    let entries = catalog();
    let item = entries
        .get(enum_name)
        .and_then(|items| {
            items.iter().find(|it| {
                it.name == item_name
                    || (enum_name == "KeyCode" && item_name == "Unknown" && it.name == "None")
            })
        })
        .ok_or_else(|| {
            mlua::Error::RuntimeError(format!("unknown Enum.{enum_name}.{item_name}"))
        })?;
    enum_type(lua, enum_name)?
        .borrow::<EnumType>()?
        .items
        .get(item.name.as_str())
}
pub fn enum_item_value(item: &AnyUserData) -> Result<i32> {
    Ok(item.borrow::<EnumItem>()?.value)
}
pub fn enum_item_matches(item: &AnyUserData, enum_type: &str) -> Result<bool> {
    Ok(item.borrow::<EnumItem>()?.enum_name == enum_type)
}
pub fn enum_item_name(item: &AnyUserData) -> Result<String> {
    Ok(item.borrow::<EnumItem>()?.name.clone())
}
fn encode(userdata: &AnyUserData) -> Result<Option<Json>> {
    match userdata.borrow::<EnumItem>() {
        Ok(item) => Ok(Some(
            json!({"$enum":{"type":item.enum_name,"name":item.name,"value":item.value}}),
        )),
        Err(_) => Ok(None),
    }
}
fn decode(lua: &Lua, value: &Json) -> Result<Value> {
    let body = value
        .get("$enum")
        .ok_or_else(|| mlua::Error::RuntimeError("invalid $enum value".into()))?;
    let name = body
        .get("type")
        .and_then(Json::as_str)
        .ok_or_else(|| mlua::Error::RuntimeError("$enum.type must be a string".into()))?;
    let expected = body
        .get("value")
        .and_then(Json::as_i64)
        .ok_or_else(|| mlua::Error::RuntimeError("$enum.value must be an integer".into()))?;
    let item_name = body
        .get("name")
        .and_then(Json::as_str)
        .map(str::to_owned)
        .or_else(|| {
            catalog()
                .get(name)?
                .iter()
                .find(|it| it.value as i64 == expected)
                .map(|it| it.name.clone())
        })
        .ok_or_else(|| mlua::Error::RuntimeError("unknown $enum value".into()))?;
    let item = enum_item(lua, name, &item_name)?;
    if enum_item_value(&item)? as i64 != expected {
        return Err(mlua::Error::RuntimeError(
            "$enum name/value mismatch".into(),
        ));
    }
    Ok(Value::UserData(item))
}

pub fn install(lua: &Lua) -> Result<()> {
    let data = catalog();
    let enums = lua.create_table_with_capacity(0, data.len())?;
    for (name, items) in data {
        let table = lua.create_table_with_capacity(items.len(), 0)?;
        for item in items {
            table.set(
                item.name.clone(),
                lua.create_userdata(EnumItem {
                    enum_name: name.clone(),
                    name: item.name.clone(),
                    value: item.value,
                })?,
            )?;
        }
        table.set_readonly(true);
        enums.set(
            name.clone(),
            lua.create_userdata(EnumType {
                name: name.clone(),
                items: table,
            })?,
        )?;
    }
    enums.set(
        "GetEnums",
        lua.create_function(|lua, _: Value| {
            let defs = catalog();
            let root: mlua::Table = lua.globals().get("Enum")?;
            let list = lua.create_table_with_capacity(defs.len(), 0)?;
            for (i, name) in defs.keys().enumerate() {
                list.raw_set(i + 1, root.get::<Value>(name.as_str())?)?;
            }
            Ok(list)
        })?,
    )?;
    enums.set_readonly(true);
    lua.globals().set("Enum", enums)?;
    super::values::register_codec(
        lua,
        super::values::Codec {
            key: "$enum",
            type_name: "EnumItem",
            decode,
            encode,
        },
    )
}
