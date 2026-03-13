let target_output = document.getElementById("measure_count");
let meter = document.getElementById("meter");
let meterReadingElement = document.getElementById("meter-reading");
let meter_value = 0
var learn_update = new EventSource("/measure");

learn_update.onmessage = function (e) {
    if (e.data == "close") {
            learn_update.close();
    } else {
            var temp_File = '<p>Current value = ' + e.data.split(",")[0] +': <br><meter value=' + e.data.split(",")[0] + ' min="0" max="1000" low="0" high="800" optimum ="500" ></meter></p>'
            target_output.innerHTML = temp_File;
            meter_value = e.data.split(",")[0];
    };
};
        
    
function flot2Int(value) {
    return value | 0
}
function getColor(val) {
    if (val <= 100) {
    meter_color = "#2db72dff";
} else if (val > 200 && val < 349) {
    meter_color = "#e5e545ff";
} else if (val >= 349) {
    meter_color = "#b45050ff";
}
return meter_color
}
function processReading(value) {
    count = flot2Int(value / 400)
    val = value - 400 * count
    color = getColor(val)
    return [count, val, color]
}

//});
startMeterAnimation();
function startMeterAnimation() {
    let timer = setInterval(() => {
        meterReadingElement.innerText = processReading(meter_value)[0];
        meter.style.background = `conic-gradient(${processReading(meter_value)[2]} ${processReading(meter_value)[1] * 0.9}deg, #fff 0deg)`;
    }, 10);
}
